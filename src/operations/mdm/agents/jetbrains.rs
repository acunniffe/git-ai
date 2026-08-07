use crate::error::GitAiError;
use crate::operations::mdm::hook_installer::{
    HookCheckResult, HookInstaller, HookInstallerParams, InstallResult, UninstallResult,
};
use crate::operations::mdm::jetbrains::{
    DetectedIde, MARKETPLACE_URL, MIN_INTELLIJ_BUILD, PLUGIN_ID, download_plugin_from_marketplace,
    find_jetbrains_installations, install_plugin_to_directory, install_plugin_via_cli,
    is_plugin_installed,
};
pub struct JetBrainsInstaller;

impl JetBrainsInstaller {
    /// Get all detected JetBrains installations
    fn get_installations() -> Vec<DetectedIde> {
        find_jetbrains_installations()
    }

    /// Try to install the plugin to a single IDE
    fn install_to_ide(detected: &DetectedIde, dry_run: bool) -> InstallResult {
        let ide_name = detected.ide.name;

        // Check if already installed
        if is_plugin_installed(detected) {
            return InstallResult {
                changed: false,
                diff: None,
                message: format!("{}: Plugin already installed", ide_name),
            };
        }

        // Check version compatibility
        if !detected.is_compatible() {
            let version_info = detected
                .build_number
                .as_deref()
                .unwrap_or("unknown version");
            return InstallResult {
                changed: false,
                diff: None,
                message: format!(
                    "{}: Skipped (build {} is older than minimum required build {})",
                    ide_name, version_info, MIN_INTELLIJ_BUILD
                ),
            };
        }

        if dry_run {
            return InstallResult {
                changed: true,
                diff: None,
                message: format!("{}: Pending plugin install", ide_name),
            };
        }

        // Try CLI installation first
        match install_plugin_via_cli(&detected.binary_path, PLUGIN_ID) {
            Ok(true) => {
                return InstallResult {
                    changed: true,
                    diff: None,
                    message: format!("{}: Plugin installed via CLI", ide_name),
                };
            }
            Ok(false) => {
                tracing::debug!(
                    "JetBrains: CLI install failed for {}, trying direct download",
                    ide_name
                );
            }
            Err(e) => {
                tracing::debug!("JetBrains: CLI install error for {}: {}", ide_name, e);
            }
        }

        // Try direct download from Marketplace
        if let Some(build_number) = &detected.build_number {
            match download_plugin_from_marketplace(
                PLUGIN_ID,
                detected.ide.product_code,
                build_number,
            ) {
                Ok(zip_data) => {
                    match install_plugin_to_directory(&zip_data, &detected.plugins_dir) {
                        Ok(()) => {
                            return InstallResult {
                                changed: true,
                                diff: None,
                                message: format!(
                                    "{}: Plugin installed from JetBrains Marketplace",
                                    ide_name
                                ),
                            };
                        }
                        Err(e) => {
                            tracing::debug!(
                                "JetBrains: Failed to extract plugin for {}: {}",
                                ide_name,
                                e
                            );
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!(
                        "JetBrains: Failed to download plugin for {}: {}",
                        ide_name,
                        e
                    );
                }
            }
        }

        // Provide manual installation URL as last resort
        InstallResult {
            changed: false,
            diff: None,
            message: format!(
                "{}: Unable to automatically install plugin. Please install manually from: {}",
                ide_name, MARKETPLACE_URL
            ),
        }
    }
}

impl HookInstaller for JetBrainsInstaller {
    fn name(&self) -> &str {
        "JetBrains IDEs"
    }

    fn id(&self) -> &str {
        "jetbrains"
    }

    fn process_names(&self) -> Vec<&str> {
        vec![
            "idea",
            "webstorm",
            "pycharm",
            "goland",
            "rustrover",
            "clion",
            "phpstorm",
            "rider",
        ]
    }

    fn uses_config_hooks(&self) -> bool {
        // JetBrains only uses install_extras for plugin installation, no config file hooks
        false
    }

    fn check_hooks(&self, _params: &HookInstallerParams) -> Result<HookCheckResult, GitAiError> {
        let installations = Self::get_installations();

        if installations.is_empty() {
            return Ok(HookCheckResult::tool_not_installed());
        }

        // Check if any compatible IDE exists
        let has_compatible = installations.iter().any(|i| i.is_compatible());

        if !has_compatible {
            tracing::debug!(
                "JetBrains: Found {} IDEs but none meet minimum version requirement (build {})",
                installations.len(),
                MIN_INTELLIJ_BUILD
            );
        }

        // JetBrains doesn't have config file hooks - only the plugin via install_extras
        // Always return hooks_installed: false so install_extras runs and shows proper messages
        Ok(HookCheckResult::installed_without_hooks())
    }

    fn install_hooks(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // JetBrains doesn't have config file hooks, only the plugin
        // The install_extras method handles the plugin installation
        Ok(None)
    }

    fn uninstall_hooks(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Option<String>, GitAiError> {
        // JetBrains doesn't have config file hooks to uninstall
        Ok(None)
    }

    fn install_extras(
        &self,
        _params: &HookInstallerParams,
        dry_run: bool,
    ) -> Result<Vec<InstallResult>, GitAiError> {
        let installations = Self::get_installations();

        if installations.is_empty() {
            return Ok(vec![InstallResult {
                changed: false,
                diff: None,
                message: "JetBrains: No IDEs detected".to_string(),
            }]);
        }

        let mut results = Vec::new();

        for detected in &installations {
            let result = Self::install_to_ide(detected, dry_run);
            results.push(result);
        }

        Ok(results)
    }

    fn uninstall_extras(
        &self,
        _params: &HookInstallerParams,
        _dry_run: bool,
    ) -> Result<Vec<UninstallResult>, GitAiError> {
        let installations = Self::get_installations();

        if installations.is_empty() {
            return Ok(vec![]);
        }

        let mut results = Vec::new();

        for detected in &installations {
            if is_plugin_installed(detected) {
                results.push(UninstallResult {
                    changed: false,
                    diff: None,
                    message: format!(
                        "{}: Plugin must be uninstalled manually through the IDE (Settings > Plugins)",
                        detected.ide.name
                    ),
                });
            }
        }

        if results.is_empty() {
            results.push(UninstallResult {
                changed: false,
                diff: None,
                message: "JetBrains: No plugins installed to uninstall".to_string(),
            });
        }

        Ok(results)
    }
}
