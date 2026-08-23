use crate::error::GitAiError;
use crate::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::mdm::utils::{binary_exists, generate_diff, home_dir, write_atomic};
use std::fs;
use std::path::{Path, PathBuf};

// OpenCode plugin content (TypeScript), embedded from the source file
const OPENCODE_PLUGIN_CONTENT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/agent-support/opencode/git-ai.ts"
));

pub struct OpenCodeInstaller;

impl OpenCodeInstaller {
    fn plugin_path() -> PathBuf {
        home_dir()
            .join(".config")
            .join("opencode")
            .join("plugins")
            .join("git-ai.ts")
    }

    /// Generate plugin content with the absolute binary path substituted in
    fn generate_plugin_content(binary_path: &Path) -> String {
        // Escape backslashes for the TypeScript string literal (needed for Windows paths)
        let path_str = binary_path.display().to_string().replace('\\', "\\\\");
        OPENCODE_PLUGIN_CONTENT.replace("__GIT_AI_BINARY_PATH__", &path_str)
    }
}

impl HookInstaller for OpenCodeInstaller {
    fn name(&self) -> &str {
        "OpenCode"
    }

    fn id(&self) -> &str {
        "opencode"
    }

    fn process_names(&self) -> Vec<&str> {
        vec!["opencode", "opencode2"]
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let has_binary = binary_exists("opencode") || binary_exists("opencode2");
        let has_global_config = home_dir().join(".config").join("opencode").exists();
        let has_local_config = Path::new(".opencode").exists();

        if !has_binary && !has_global_config && !has_local_config {
            return Ok(HookCheckResult {
                tool_installed: false,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // Check if plugin is installed
        let plugin_path = Self::plugin_path();
        if !plugin_path.exists() {
            return Ok(HookCheckResult {
                tool_installed: true,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        // Check if plugin is up to date (compare against content with binary path substituted)
        let current_content = fs::read_to_string(&plugin_path).unwrap_or_default();
        let expected_content = Self::generate_plugin_content(&params.binary_path);
        let is_up_to_date = current_content.trim() == expected_content.trim();

        Ok(HookCheckResult {
            tool_installed: true,
            hooks_installed: true,
            hooks_up_to_date: is_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let plugin_path = Self::plugin_path();

        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let legacy_path = home_dir()
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            let _ = fs::remove_file(&legacy_path);
        }

        // Ensure directory exists
        if let Some(dir) = plugin_path.parent()
            && !dry_run
        {
            fs::create_dir_all(dir)?;
        }

        // Read existing content if present
        let existing_content = if plugin_path.exists() {
            fs::read_to_string(&plugin_path)?
        } else {
            String::new()
        };

        let new_content = Self::generate_plugin_content(&params.binary_path);

        // Check if there are changes
        if existing_content.trim() == new_content.trim() {
            return Ok(None);
        }

        // Generate diff
        let diff_output = generate_diff(&plugin_path, &existing_content, &new_content);

        // Write if not dry-run
        if !dry_run {
            // Ensure directory exists (might not exist in dry run check above)
            if let Some(dir) = plugin_path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(&plugin_path, new_content.as_bytes())?;
        }

        Ok(Some(diff_output))
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let plugin_path = Self::plugin_path();

        // Remove legacy plugin from old installations (~/.config/opencode/plugin/ singular)
        if !dry_run {
            let legacy_path = home_dir()
                .join(".config")
                .join("opencode")
                .join("plugin")
                .join("git-ai.ts");
            let _ = fs::remove_file(&legacy_path);
        }

        if !plugin_path.exists() {
            return Ok(None);
        }

        let existing_content = fs::read_to_string(&plugin_path)?;
        let diff_output = generate_diff(&plugin_path, &existing_content, "");

        if !dry_run {
            fs::remove_file(&plugin_path)?;
        }

        Ok(Some(diff_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path().to_path_buf();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
        }

        f(&home);

        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
        }
    }

    fn with_fake_binary_on_path<F: FnOnce(&Path)>(binary_name: &str, f: F) {
        let temp_dir = TempDir::new().unwrap();
        let bin_dir = temp_dir.path().join("bin");
        fs::create_dir_all(&bin_dir).unwrap();
        let fake_bin = bin_dir.join(binary_name);
        fs::write(&fake_bin, "#!/bin/sh\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_bin, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let prev_path = std::env::var_os("PATH");
        let new_path = match &prev_path {
            Some(p) => {
                let mut paths = vec![bin_dir.clone()];
                paths.extend(std::env::split_paths(p));
                std::env::join_paths(paths).unwrap()
            }
            None => bin_dir.clone().into(),
        };

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        f(temp_dir.path());

        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
    }

    fn with_empty_path<F: FnOnce()>(f: F) {
        let temp_dir = TempDir::new().unwrap();
        let prev_path = std::env::var_os("PATH");

        // SAFETY: tests are serialized via #[serial], so mutating process env is safe.
        unsafe {
            std::env::set_var("PATH", temp_dir.path());
        }

        f();

        // SAFETY: tests are serialized via #[serial], so restoring process env is safe.
        unsafe {
            match prev_path {
                Some(v) => std::env::set_var("PATH", v),
                None => std::env::remove_var("PATH"),
            }
        }
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
        assert!(content.contains("execute.before"));
        assert!(content.contains("execute.after"));
        // Uses the opencode preset with session_id-based hook input and absolute path
        assert!(content.contains("session_id"));
        // Placeholder should be replaced with actual binary path in the const declaration
        assert!(!content.contains("__GIT_AI_BINARY_PATH__"));
        assert!(content.contains(r#"const GIT_AI_BIN = "/usr/local/bin/git-ai""#));
    }

    #[test]
    fn test_opencode_plugin_content_is_valid_typescript() {
        let content = OPENCODE_PLUGIN_CONTENT;

        assert!(content.contains("import { Plugin }"));
        assert!(content.contains("@opencode-ai/plugin"));
        assert!(content.contains("export const GitAiPlugin = Plugin.define("));
        assert!(content.contains("export default GitAiPlugin"));
        assert!(content.contains("child_process"));
        assert!(content.contains("\"execute.before\""));
        assert!(content.contains("\"execute.after\""));
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
}
