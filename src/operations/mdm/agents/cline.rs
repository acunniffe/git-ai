use crate::error::GitAiError;
use crate::operations::mdm::file_ops::{generate_diff, write_atomic};
use crate::operations::mdm::hook_installer::{HookCheckResult, HookInstaller, HookInstallerParams};
use crate::operations::mdm::paths::{home_dir, normalize_windows_path_for_shell};
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
#[cfg(any(target_os = "macos", test))]
use std::sync::mpsc;
#[cfg(any(target_os = "macos", test))]
use std::thread;
#[cfg(any(target_os = "macos", test))]
use std::time::Duration;

pub struct ClineInstaller;

const MANAGED_MARKER: &str = "# git-ai-managed: cline";
const PRE_HOOK_NAME: &str = "PreToolUse";
const POST_HOOK_NAME: &str = "PostToolUse";
const CLINE_PUBLISHER_ID: &str = "saoudrizwan.claude-dev";
#[cfg(target_os = "macos")]
const CLINE_DOCUMENTS_ACCESS_TIMEOUT: Duration = Duration::from_secs(30);

impl ClineInstaller {
    fn storage_paths() -> Vec<PathBuf> {
        if let Ok(test_path) = std::env::var("GIT_AI_CLINE_STORAGE_PATH") {
            return vec![PathBuf::from(test_path)];
        }

        let products = ["Code", "Code - Insiders", "Cursor"];

        #[cfg(target_os = "macos")]
        {
            let base = home_dir().join("Library").join("Application Support");
            products
                .iter()
                .map(|p| {
                    base.join(p)
                        .join("User")
                        .join("globalStorage")
                        .join(CLINE_PUBLISHER_ID)
                })
                .collect()
        }

        #[cfg(target_os = "linux")]
        {
            let base = home_dir().join(".config");
            products
                .iter()
                .map(|p| {
                    base.join(p)
                        .join("User")
                        .join("globalStorage")
                        .join(CLINE_PUBLISHER_ID)
                })
                .collect()
        }

        #[cfg(target_os = "windows")]
        {
            let base = if let Ok(app_data) = std::env::var("APPDATA") {
                PathBuf::from(app_data)
            } else {
                home_dir().join("AppData").join("Roaming")
            };
            products
                .iter()
                .map(|p| {
                    base.join(p)
                        .join("User")
                        .join("globalStorage")
                        .join(CLINE_PUBLISHER_ID)
                })
                .collect()
        }

        #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
        {
            vec![]
        }
    }

    fn hooks_dir() -> PathBuf {
        home_dir().join("Documents").join("Cline").join("Hooks")
    }

    fn hook_path(name: &str) -> PathBuf {
        Self::hooks_dir().join(name)
    }

    fn generate_hook_script(binary_path: &Path) -> String {
        let binary = normalize_windows_path_for_shell(binary_path);
        format!(
            "#!/bin/sh\n{}\n\"{}\" checkpoint cline --hook-input stdin\necho '{{\"cancel\":false}}'\n",
            MANAGED_MARKER, binary
        )
    }

    fn is_managed_script(content: &str) -> bool {
        content
            .lines()
            .any(|line| line.trim() == MANAGED_MARKER.trim())
    }

    fn read_hook_script(path: &Path) -> Result<Option<String>, GitAiError> {
        match fs::read_to_string(path) {
            Ok(content) => Ok(Some(content)),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(None),
            Err(error) => Err(GitAiError::Generic(format!(
                "Unable to access Cline hooks at {}: {}",
                path.display(),
                error
            ))),
        }
    }

    fn inspect_hook_scripts(binary_path: &Path) -> Result<(bool, bool), GitAiError> {
        let pre = Self::read_hook_script(&Self::hook_path(PRE_HOOK_NAME))?;
        let post = Self::read_hook_script(&Self::hook_path(POST_HOOK_NAME))?;
        let pre_managed = pre.as_deref().map(Self::is_managed_script).unwrap_or(false);
        let post_managed = post
            .as_deref()
            .map(Self::is_managed_script)
            .unwrap_or(false);
        let hooks_installed = pre_managed || post_managed;

        let expected = Self::generate_hook_script(binary_path);
        let hooks_up_to_date = pre_managed
            && post_managed
            && pre
                .as_deref()
                .is_some_and(|content| content.trim() == expected.trim())
            && post
                .as_deref()
                .is_some_and(|content| content.trim() == expected.trim());

        Ok((hooks_installed, hooks_up_to_date))
    }

    #[cfg(target_os = "macos")]
    fn preflight_documents_access() -> Result<(), GitAiError> {
        let documents = home_dir().join("Documents");
        fs::read_dir(&documents).map(drop).map_err(|error| {
            GitAiError::Generic(format!(
                "Unable to access Cline hooks in {}: {}",
                documents.display(),
                error
            ))
        })
    }

    #[cfg(any(target_os = "macos", test))]
    fn run_hook_check_with_timeout<T, F>(timeout: Duration, check: F) -> Result<T, GitAiError>
    where
        T: Send + 'static,
        F: FnOnce() -> Result<T, GitAiError> + Send + 'static,
    {
        let (result_tx, result_rx) = mpsc::sync_channel(1);
        let _ = thread::spawn(move || {
            let _ = result_tx.send(check());
        });

        match result_rx.recv_timeout(timeout) {
            Ok(result) => result,
            Err(mpsc::RecvTimeoutError::Timeout) => Err(GitAiError::Generic(format!(
                "Timed out checking Cline hooks after {} seconds; Cline hooks were not changed",
                timeout.as_secs()
            ))),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(GitAiError::Generic(
                "Cline hook check stopped unexpectedly".to_string(),
            )),
        }
    }

    fn is_windows() -> bool {
        cfg!(target_os = "windows")
    }

    fn ensure_hook_script_is_writable(path: &Path) -> Result<(), GitAiError> {
        if let Some(content) = Self::read_hook_script(path)?
            && !Self::is_managed_script(&content)
        {
            return Err(GitAiError::Generic(format!(
                "Refusing to overwrite unmanaged Cline hook: {}",
                path.display()
            )));
        }

        Ok(())
    }

    fn install_hook_script(
        path: &Path,
        content: &str,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        let existing = Self::read_hook_script(path)?.unwrap_or_default();

        if existing.trim() == content.trim() {
            return Ok(None);
        }

        let diff = generate_diff(path, &existing, content);

        if !dry_run {
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir)?;
            }
            write_atomic(path, content.as_bytes())?;

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(path, fs::Permissions::from_mode(0o755))?;
            }
        }

        Ok(Some(diff))
    }

    fn uninstall_hook_script(path: &Path, dry_run: bool) -> Result<Option<String>, GitAiError> {
        let Some(existing) = Self::read_hook_script(path)? else {
            return Ok(None);
        };
        if !Self::is_managed_script(&existing) {
            return Ok(None);
        }

        let diff = generate_diff(path, &existing, "");

        if !dry_run {
            fs::remove_file(path)?;
        }

        Ok(Some(diff))
    }
}

impl HookInstaller for ClineInstaller {
    fn name(&self) -> &str {
        "Cline"
    }

    fn id(&self) -> &str {
        "cline"
    }

    fn process_names(&self) -> Vec<&str> {
        vec![]
    }

    fn uses_config_hooks(&self) -> bool {
        true
    }

    fn check_hooks(&self, params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let tool_installed = Self::storage_paths().iter().any(|p| p.exists());

        if !tool_installed || Self::is_windows() {
            // Cline hooks are not supported on Windows today; report the tool if it
            // is installed but leave hooks uninstalled.
            return Ok(HookCheckResult {
                tool_installed,
                hooks_installed: false,
                hooks_up_to_date: false,
            });
        }

        #[cfg(target_os = "macos")]
        let (hooks_installed, hooks_up_to_date) = {
            let binary_path = params.binary_path.clone();
            Self::run_hook_check_with_timeout(CLINE_DOCUMENTS_ACCESS_TIMEOUT, move || {
                Self::preflight_documents_access()?;
                Self::inspect_hook_scripts(&binary_path)
            })?
        };

        #[cfg(not(target_os = "macos"))]
        let (hooks_installed, hooks_up_to_date) = Self::inspect_hook_scripts(&params.binary_path)?;

        Ok(HookCheckResult {
            tool_installed,
            hooks_installed,
            hooks_up_to_date,
        })
    }

    fn install_hooks(
        &self,
        params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if Self::is_windows() {
            return Ok(None);
        }

        let pre_path = Self::hook_path(PRE_HOOK_NAME);
        let post_path = Self::hook_path(POST_HOOK_NAME);
        Self::ensure_hook_script_is_writable(&pre_path)?;
        Self::ensure_hook_script_is_writable(&post_path)?;

        if !dry_run {
            fs::create_dir_all(Self::hooks_dir())?;
        }

        let script = Self::generate_hook_script(&params.binary_path);

        let pre_diff = Self::install_hook_script(&pre_path, &script, dry_run)?;
        let post_diff = Self::install_hook_script(&post_path, &script, dry_run)?;

        match (pre_diff, post_diff) {
            (None, None) => Ok(None),
            (Some(a), None) => Ok(Some(a)),
            (None, Some(b)) => Ok(Some(b)),
            (Some(a), Some(b)) => Ok(Some(format!("{}\n{}", a, b))),
        }
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        if Self::is_windows() {
            return Ok(None);
        }

        let pre_path = Self::hook_path(PRE_HOOK_NAME);
        let post_path = Self::hook_path(POST_HOOK_NAME);

        let pre_diff = Self::uninstall_hook_script(&pre_path, dry_run)?;
        let post_diff = Self::uninstall_hook_script(&post_path, dry_run)?;

        match (pre_diff, post_diff) {
            (None, None) => Ok(None),
            (Some(a), None) => Ok(Some(a)),
            (None, Some(b)) => Ok(Some(b)),
            (Some(a), Some(b)) => Ok(Some(format!("{}\n{}", a, b))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;
    use std::fs;
    use std::sync::mpsc;
    use std::time::Duration;
    use tempfile::TempDir;

    fn create_test_binary_path() -> PathBuf {
        PathBuf::from("/usr/local/bin/git-ai")
    }

    fn with_temp_home<F: FnOnce(&Path)>(f: F) {
        let temp_dir = TempDir::new().unwrap();
        let home = temp_dir.path().to_path_buf();

        let prev_home = std::env::var_os("HOME");
        let prev_userprofile = std::env::var_os("USERPROFILE");
        let prev_storage = std::env::var_os("GIT_AI_CLINE_STORAGE_PATH");

        unsafe {
            std::env::set_var("HOME", &home);
            std::env::set_var("USERPROFILE", &home);
        }

        f(&home);

        unsafe {
            match prev_home {
                Some(v) => std::env::set_var("HOME", v),
                None => std::env::remove_var("HOME"),
            }
            match prev_userprofile {
                Some(v) => std::env::set_var("USERPROFILE", v),
                None => std::env::remove_var("USERPROFILE"),
            }
            match prev_storage {
                Some(v) => std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", v),
                None => std::env::remove_var("GIT_AI_CLINE_STORAGE_PATH"),
            }
        }
    }

    #[test]
    #[serial]
    fn test_cline_check_not_installed() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            let result = ClineInstaller.check_hooks(&params).unwrap();
            assert!(!result.tool_installed);
            assert!(!result.hooks_installed);
            assert!(!result.hooks_up_to_date);
        });
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn test_cline_check_does_not_inspect_hooks_when_tool_is_absent() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            fs::create_dir_all(ClineInstaller::hooks_dir()).unwrap();
            fs::write(
                ClineInstaller::hook_path(PRE_HOOK_NAME),
                format!("#!/bin/sh\n{}\n", MANAGED_MARKER),
            )
            .unwrap();

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            let result = ClineInstaller.check_hooks(&params).unwrap();

            assert!(!result.tool_installed);
            assert!(!result.hooks_installed);
            assert!(!result.hooks_up_to_date);
        });
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn test_cline_check_propagates_hook_path_access_errors() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            fs::write(home.join("Documents"), "not a directory").unwrap();

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            let error = match ClineInstaller.check_hooks(&params) {
                Ok(_) => panic!("expected Cline hook access to fail"),
                Err(error) => error,
            };

            assert!(
                error.to_string().contains("Cline hooks"),
                "unexpected error: {error}"
            );
        });
    }

    #[test]
    fn test_cline_hook_check_timeout_is_bounded() {
        let (release_tx, release_rx) = mpsc::channel();
        let result: Result<(), GitAiError> =
            ClineInstaller::run_hook_check_with_timeout(Duration::from_millis(10), move || {
                release_rx.recv().unwrap();
                Ok(())
            });

        let error = match result {
            Ok(()) => panic!("expected Cline hook check to time out"),
            Err(error) => error,
        };
        assert!(
            error.to_string().contains("Timed out checking Cline hooks"),
            "unexpected error: {error}"
        );

        release_tx.send(()).unwrap();
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn test_cline_install_creates_hooks() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = ClineInstaller.install_hooks(&params, false).unwrap();
            assert!(result.is_some(), "expected a diff");

            let pre_path = ClineInstaller::hook_path(PRE_HOOK_NAME);
            let post_path = ClineInstaller::hook_path(POST_HOOK_NAME);

            assert!(pre_path.exists());
            assert!(post_path.exists());

            let content = fs::read_to_string(&pre_path).unwrap();
            assert!(content.contains("git-ai-managed"));
            assert!(content.contains("checkpoint cline"));
            assert!(content.contains(r#"{"cancel":false}"#));

            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = fs::metadata(&pre_path).unwrap().permissions().mode();
                assert_eq!(mode & 0o111, 0o111, "hook should be executable");
            }

            let check = ClineInstaller.check_hooks(&params).unwrap();
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(check.hooks_up_to_date);
        });
    }

    #[test]
    #[serial]
    fn test_cline_install_is_idempotent() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            ClineInstaller.install_hooks(&params, false).unwrap();
            let second = ClineInstaller.install_hooks(&params, false).unwrap();
            assert!(second.is_none(), "second install should be a no-op");
        });
    }

    #[test]
    #[serial]
    fn test_cline_uninstall_removes_hooks() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            ClineInstaller.install_hooks(&params, false).unwrap();
            ClineInstaller.uninstall_hooks(&params, false).unwrap();

            assert!(!ClineInstaller::hook_path(PRE_HOOK_NAME).exists());
            assert!(!ClineInstaller::hook_path(POST_HOOK_NAME).exists());

            let check = ClineInstaller.check_hooks(&params).unwrap();
            assert!(check.tool_installed);
            assert!(!check.hooks_installed);
        });
    }

    #[test]
    #[serial]
    fn test_cline_uninstall_preserves_unmanaged_files() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            fs::create_dir_all(ClineInstaller::hooks_dir()).unwrap();
            let pre_path = ClineInstaller::hook_path(PRE_HOOK_NAME);
            fs::write(&pre_path, "#!/bin/sh\necho 'user hook'\n").unwrap();

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let result = ClineInstaller.uninstall_hooks(&params, false).unwrap();
            assert!(result.is_none());
            assert!(pre_path.exists());
        });
    }

    #[test]
    #[cfg(not(windows))]
    #[serial]
    fn test_cline_install_preserves_unmanaged_hook_without_partial_updates() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            fs::create_dir_all(ClineInstaller::hooks_dir()).unwrap();
            let pre_path = ClineInstaller::hook_path(PRE_HOOK_NAME);
            let post_path = ClineInstaller::hook_path(POST_HOOK_NAME);
            let managed_pre = format!("#!/bin/sh\n{}\necho 'stale'\n", MANAGED_MARKER);
            let unmanaged_post = "#!/bin/sh\necho 'user hook'\n";
            fs::write(&pre_path, &managed_pre).unwrap();
            fs::write(&post_path, unmanaged_post).unwrap();

            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };

            let error = ClineInstaller.install_hooks(&params, false).unwrap_err();
            assert!(error.to_string().contains("unmanaged Cline hook"));
            assert_eq!(fs::read_to_string(&pre_path).unwrap(), managed_pre);
            assert_eq!(fs::read_to_string(&post_path).unwrap(), unmanaged_post);
        });
    }

    #[test]
    #[serial]
    fn test_cline_partial_managed_install_is_detected_and_uninstalled() {
        with_temp_home(|home| {
            let storage = home.join("cline-storage");
            fs::create_dir_all(&storage).unwrap();
            unsafe { std::env::set_var("GIT_AI_CLINE_STORAGE_PATH", &storage) };

            fs::create_dir_all(ClineInstaller::hooks_dir()).unwrap();
            let params = HookInstallerParams {
                binary_path: create_test_binary_path(),
            };
            fs::write(
                ClineInstaller::hook_path(PRE_HOOK_NAME),
                ClineInstaller::generate_hook_script(&params.binary_path),
            )
            .unwrap();
            fs::write(
                ClineInstaller::hook_path(POST_HOOK_NAME),
                "#!/bin/sh\necho 'user hook'\n",
            )
            .unwrap();

            let check = ClineInstaller.check_hooks(&params).unwrap();
            assert!(check.tool_installed);
            assert!(check.hooks_installed);
            assert!(!check.hooks_up_to_date);

            ClineInstaller.uninstall_hooks(&params, false).unwrap();
            assert!(!ClineInstaller::hook_path(PRE_HOOK_NAME).exists());
            assert_eq!(
                fs::read_to_string(ClineInstaller::hook_path(POST_HOOK_NAME)).unwrap(),
                "#!/bin/sh\necho 'user hook'\n"
            );
        });
    }
}
