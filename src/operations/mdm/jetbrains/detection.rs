use super::ide_types::{DetectedIde, JETBRAINS_IDES, JetBrainsIde};
use crate::operations::mdm::paths::home_dir;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;
#[cfg(windows)]
use winreg::{
    RegKey,
    enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE},
};

/// Find all installed JetBrains IDEs on the system
pub fn find_jetbrains_installations() -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    #[cfg(target_os = "macos")]
    {
        detected.extend(find_macos_installations());
    }

    #[cfg(windows)]
    {
        detected.extend(find_windows_installations());
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        detected.extend(find_linux_installations());
    }

    detected
}

// ===== macOS Detection =====

#[cfg(target_os = "macos")]
fn find_macos_installations() -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    for ide in JETBRAINS_IDES {
        for bundle_id in ide.bundle_ids {
            if let Some(app_path) = find_app_by_bundle_id(bundle_id)
                && let Some(detected_ide) = detect_macos_ide(ide, &app_path)
            {
                detected.push(detected_ide);
            }
        }
    }

    // Also scan common installation directories
    let scan_dirs = vec![
        PathBuf::from("/Applications"),
        home_dir().join("Applications"),
        home_dir().join("Applications/JetBrains Toolbox"),
    ];

    for scan_dir in scan_dirs {
        if scan_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&scan_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "app") {
                    for ide in JETBRAINS_IDES {
                        if is_matching_macos_app(ide, &path)
                            && let Some(detected_ide) = detect_macos_ide(ide, &path)
                        {
                            // Avoid duplicates
                            if !detected
                                .iter()
                                .any(|d| d.install_path == detected_ide.install_path)
                            {
                                detected.push(detected_ide);
                            }
                        }
                    }
                }
            }
        }
    }

    detected
}

#[cfg(target_os = "macos")]
fn find_app_by_bundle_id(bundle_id: &str) -> Option<PathBuf> {
    let output = Command::new("mdfind")
        .args([&format!("kMDItemCFBundleIdentifier == '{}'", bundle_id)])
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.lines().next().map(PathBuf::from)
}

#[cfg(target_os = "macos")]
fn is_matching_macos_app(ide: &JetBrainsIde, app_path: &Path) -> bool {
    let app_name = app_path.file_stem().and_then(|s| s.to_str()).unwrap_or("");

    let app_name_lower = app_name.to_lowercase();

    // Match based on IDE name patterns
    match ide.product_code {
        "IU" | "IC" => app_name_lower.contains("intellij"),
        "PY" | "PC" => app_name_lower.contains("pycharm"),
        "WS" => app_name_lower.contains("webstorm"),
        "GO" => app_name_lower.contains("goland"),
        "CL" => app_name_lower.contains("clion"),
        "PS" => app_name_lower.contains("phpstorm"),
        "RD" => app_name_lower.contains("rider"),
        "RM" => app_name_lower.contains("rubymine"),
        "DB" => app_name_lower.contains("datagrip"),
        "AI" => app_name_lower.contains("android studio"),
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn detect_macos_ide(ide: &'static JetBrainsIde, app_path: &Path) -> Option<DetectedIde> {
    let binary_path = app_path
        .join("Contents")
        .join("MacOS")
        .join(ide.binary_name_macos);

    if !binary_path.exists() {
        tracing::debug!(
            "JetBrains: Binary not found at {:?} for {}",
            binary_path,
            ide.name
        );
        return None;
    }

    // Get build number and data directory from Info.plist or product-info.json
    let (build_number, major_build, data_directory_name) = get_macos_build_metadata(app_path);

    // Get plugins directory
    let plugins_dir = get_plugins_dir(
        data_directory_name.as_deref(),
        ide.product_code,
        build_number.as_deref(),
    );

    Some(DetectedIde {
        ide,
        install_path: app_path.to_path_buf(),
        binary_path,
        build_number,
        major_build,
        plugins_dir,
    })
}

#[cfg(target_os = "macos")]
fn get_macos_build_metadata(app_path: &Path) -> (Option<String>, Option<u32>, Option<String>) {
    // Try product-info.json first (newer JetBrains IDEs)
    let product_info_path = app_path.join("Contents/Resources/product-info.json");
    if product_info_path.exists()
        && let Ok(content) = std::fs::read_to_string(&product_info_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(build) = json.get("buildNumber").and_then(|v| v.as_str())
    {
        let major = parse_major_build(build);
        let data_directory_name = json
            .get("dataDirectoryName")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        return (Some(build.to_string()), major, data_directory_name);
    }

    // Fall back to Info.plist
    let output = Command::new("defaults")
        .args([
            "read",
            &app_path.join("Contents/Info.plist").to_string_lossy(),
            "CFBundleVersion",
        ])
        .output()
        .ok();

    if let Some(output) = output
        && output.status.success()
    {
        let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
        let major = parse_major_build(&version);
        return (Some(version), major, None);
    }

    (None, None, None)
}

// ===== Windows Detection =====

#[cfg(windows)]
fn find_windows_installations() -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    // Scan Toolbox directory
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        let toolbox_apps = PathBuf::from(&local_app_data)
            .join("JetBrains")
            .join("Toolbox")
            .join("apps");

        if toolbox_apps.exists() {
            detected.extend(scan_windows_toolbox_dir(&toolbox_apps));
        }
    }

    // Scan Program Files directories for JetBrains IDEs and the default Android Studio install.
    let program_dirs = windows_program_files_dirs();

    for program_dir in &program_dirs {
        let jetbrains_dir = program_dir.join("JetBrains");
        if jetbrains_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&jetbrains_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    for ide in JETBRAINS_IDES {
                        if let Some(detected_ide) = detect_windows_ide(ide, &path)
                            && !detected
                                .iter()
                                .any(|d| d.install_path == detected_ide.install_path)
                        {
                            detected.push(detected_ide);
                        }
                    }
                }
            }
        }
    }

    let android_studio = android_studio_ide();
    for install_path in windows_android_studio_installation_candidates(&program_dirs) {
        if let Some(detected_ide) = detect_windows_ide(android_studio, &install_path)
            && !detected
                .iter()
                .any(|d| d.install_path == detected_ide.install_path)
        {
            detected.push(detected_ide);
        }
    }

    detected
}

#[cfg(windows)]
fn scan_windows_toolbox_dir(toolbox_apps: &Path) -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    if let Ok(entries) = std::fs::read_dir(toolbox_apps) {
        for entry in entries.flatten() {
            let app_dir = entry.path();
            if !app_dir.is_dir() {
                continue;
            }

            // Find matching IDE by toolbox app name
            let dir_name = app_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");

            for ide in JETBRAINS_IDES {
                if dir_name.contains(ide.toolbox_app_name)
                    && let Ok(versions) = std::fs::read_dir(&app_dir)
                {
                    for version_entry in versions.flatten() {
                        let version_dir = version_entry.path();
                        if version_dir.is_dir()
                            && let Some(detected_ide) = detect_windows_ide(ide, &version_dir)
                        {
                            detected.push(detected_ide);
                        }
                    }
                }
            }
        }
    }

    detected
}

#[cfg(windows)]
fn detect_windows_ide(ide: &'static JetBrainsIde, install_path: &Path) -> Option<DetectedIde> {
    let binary_path = install_path.join("bin").join(ide.binary_name_windows);

    if !binary_path.exists() {
        return None;
    }

    let (build_number, major_build, data_directory_name) = get_windows_build_metadata(install_path);
    let plugins_dir = get_plugins_dir(
        data_directory_name.as_deref(),
        ide.product_code,
        build_number.as_deref(),
    );

    Some(DetectedIde {
        ide,
        install_path: install_path.to_path_buf(),
        binary_path,
        build_number,
        major_build,
        plugins_dir,
    })
}

#[cfg(windows)]
fn get_windows_build_metadata(
    install_path: &Path,
) -> (Option<String>, Option<u32>, Option<String>) {
    let product_info_path = install_path.join("product-info.json");
    if product_info_path.exists()
        && let Ok(content) = std::fs::read_to_string(&product_info_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(build) = json.get("buildNumber").and_then(|v| v.as_str())
    {
        let major = parse_major_build(build);
        let data_directory_name = json
            .get("dataDirectoryName")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        return (Some(build.to_string()), major, data_directory_name);
    }

    (None, None, None)
}

// ===== Linux Detection =====

#[cfg(all(unix, not(target_os = "macos")))]
fn find_linux_installations() -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    // Scan Toolbox directory
    let toolbox_apps = home_dir()
        .join(".local")
        .join("share")
        .join("JetBrains")
        .join("Toolbox")
        .join("apps");

    if toolbox_apps.exists() {
        detected.extend(scan_linux_toolbox_dir(&toolbox_apps));
    }

    // Scan common installation directories
    let scan_dirs = vec![
        home_dir().join(".local").join("share").join("JetBrains"),
        PathBuf::from("/opt"),
        PathBuf::from("/usr/local"),
    ];

    for scan_dir in scan_dirs {
        if scan_dir.exists()
            && let Ok(entries) = std::fs::read_dir(&scan_dir)
        {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    for ide in JETBRAINS_IDES {
                        if let Some(detected_ide) = detect_linux_ide(ide, &path)
                            && !detected
                                .iter()
                                .any(|d| d.install_path == detected_ide.install_path)
                        {
                            detected.push(detected_ide);
                        }
                    }
                }
            }
        }
    }

    detected
}

#[cfg(all(unix, not(target_os = "macos")))]
fn scan_linux_toolbox_dir(toolbox_apps: &Path) -> Vec<DetectedIde> {
    let mut detected = Vec::new();

    if let Ok(entries) = std::fs::read_dir(toolbox_apps) {
        for entry in entries.flatten() {
            let app_dir = entry.path();
            if !app_dir.is_dir() {
                continue;
            }

            let dir_name = app_dir.file_name().and_then(|s| s.to_str()).unwrap_or("");

            for ide in JETBRAINS_IDES {
                if dir_name.contains(ide.toolbox_app_name) {
                    // Toolbox uses versioned subdirectories with a "ch-0" pattern
                    if let Ok(channels) = std::fs::read_dir(&app_dir) {
                        for channel_entry in channels.flatten() {
                            let channel_dir = channel_entry.path();
                            if channel_dir.is_dir() {
                                // Inside channel, there are version directories
                                if let Ok(versions) = std::fs::read_dir(&channel_dir) {
                                    for version_entry in versions.flatten() {
                                        let version_dir = version_entry.path();
                                        if version_dir.is_dir()
                                            && let Some(detected_ide) =
                                                detect_linux_ide(ide, &version_dir)
                                        {
                                            detected.push(detected_ide);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    detected
}

#[cfg(all(unix, not(target_os = "macos")))]
fn detect_linux_ide(ide: &'static JetBrainsIde, install_path: &Path) -> Option<DetectedIde> {
    let binary_path = install_path.join("bin").join(ide.binary_name_linux);

    if !binary_path.exists() {
        return None;
    }

    let (build_number, major_build, data_directory_name) = get_linux_build_metadata(install_path);
    let plugins_dir = get_plugins_dir(
        data_directory_name.as_deref(),
        ide.product_code,
        build_number.as_deref(),
    );

    Some(DetectedIde {
        ide,
        install_path: install_path.to_path_buf(),
        binary_path,
        build_number,
        major_build,
        plugins_dir,
    })
}

#[cfg(all(unix, not(target_os = "macos")))]
fn get_linux_build_metadata(install_path: &Path) -> (Option<String>, Option<u32>, Option<String>) {
    let product_info_path = install_path.join("product-info.json");
    if product_info_path.exists()
        && let Ok(content) = std::fs::read_to_string(&product_info_path)
        && let Ok(json) = serde_json::from_str::<serde_json::Value>(&content)
        && let Some(build) = json.get("buildNumber").and_then(|v| v.as_str())
    {
        let major = parse_major_build(build);
        let data_directory_name = json
            .get("dataDirectoryName")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned);
        return (Some(build.to_string()), major, data_directory_name);
    }

    (None, None, None)
}

// ===== Shared Utilities =====

#[cfg_attr(not(test), allow(dead_code))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JetBrainsPlatform {
    Macos,
    Windows,
    Linux,
}

/// Parse the major build number from a build string like "252.12345.67"
fn parse_major_build(build: &str) -> Option<u32> {
    build.split('.').next()?.parse().ok()
}

fn plugin_version_suffix(
    data_directory_name: Option<&str>,
    product_code: &str,
    build_number: Option<&str>,
) -> String {
    // Prefer the IDE's real dataDirectoryName from product-info.json when available.
    // This matches the actual config/plugins directory used by modern JetBrains IDEs
    // (for example "IntelliJIdea2026.1"), avoiding incorrect guesses like "IU2026.1".
    data_directory_name
        .map(ToOwned::to_owned)
        .or_else(|| {
            build_number.and_then(parse_major_build).map(|major| {
                // Build 252 = 2025.2, 251 = 2025.1, 243 = 2024.3, etc.
                let year = 2000 + (major / 10);
                let minor = major % 10;
                format!("{}{}.{}", product_code, year, minor)
            })
        })
        .unwrap_or_else(|| product_code.to_string())
}

fn plugins_parent_dir_name(product_code: &str) -> &'static str {
    match product_code {
        // Android Studio stores its user directories under Google instead of JetBrains.
        "AI" => "Google",
        _ => "JetBrains",
    }
}

fn plugins_dir_for_platform(
    platform: JetBrainsPlatform,
    home_dir: &Path,
    appdata: Option<&Path>,
    data_directory_name: Option<&str>,
    product_code: &str,
    build_number: Option<&str>,
) -> PathBuf {
    let version_suffix = plugin_version_suffix(data_directory_name, product_code, build_number);
    let parent_dir = plugins_parent_dir_name(product_code);

    match platform {
        JetBrainsPlatform::Macos => home_dir
            .join("Library")
            .join("Application Support")
            .join(parent_dir)
            .join(&version_suffix)
            .join("plugins"),
        JetBrainsPlatform::Windows => appdata
            .map(Path::to_path_buf)
            .unwrap_or_else(|| home_dir.join("AppData").join("Roaming"))
            .join(parent_dir)
            .join(&version_suffix)
            .join("plugins"),
        // Linux stores user-installed plugins directly in the share directory root.
        JetBrainsPlatform::Linux => home_dir
            .join(".local")
            .join("share")
            .join(parent_dir)
            .join(&version_suffix),
    }
}

/// Get the plugins directory for an IDE
fn get_plugins_dir(
    data_directory_name: Option<&str>,
    product_code: &str,
    build_number: Option<&str>,
) -> PathBuf {
    let home = home_dir();

    #[cfg(target_os = "macos")]
    {
        plugins_dir_for_platform(
            JetBrainsPlatform::Macos,
            &home,
            None,
            data_directory_name,
            product_code,
            build_number,
        )
    }

    #[cfg(windows)]
    {
        let appdata = std::env::var("APPDATA").ok();
        plugins_dir_for_platform(
            JetBrainsPlatform::Windows,
            &home,
            appdata.as_deref().map(Path::new),
            data_directory_name,
            product_code,
            build_number,
        )
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        plugins_dir_for_platform(
            JetBrainsPlatform::Linux,
            &home,
            None,
            data_directory_name,
            product_code,
            build_number,
        )
    }
}

#[cfg(windows)]
fn windows_program_files_dirs() -> Vec<PathBuf> {
    [
        std::env::var("ProgramFiles").ok(),
        std::env::var("ProgramFiles(x86)").ok(),
    ]
    .into_iter()
    .flatten()
    .map(PathBuf::from)
    .collect()
}

#[cfg(windows)]
fn android_studio_ide() -> &'static JetBrainsIde {
    JETBRAINS_IDES
        .iter()
        .find(|ide| ide.product_code == "AI")
        .expect("Android Studio must remain in JETBRAINS_IDES")
}

#[cfg(windows)]
fn windows_android_studio_installation_candidates(program_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut candidates = default_windows_android_studio_install_paths(program_dirs);
    for candidate in read_windows_android_studio_registry_candidates() {
        push_unique_path(&mut candidates, candidate);
    }
    candidates
}

#[cfg(windows)]
fn default_windows_android_studio_install_paths(program_dirs: &[PathBuf]) -> Vec<PathBuf> {
    program_dirs
        .iter()
        .map(|program_dir| program_dir.join("Android").join("Android Studio"))
        .collect()
}

#[cfg(windows)]
fn read_windows_android_studio_registry_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    for hive in [
        RegKey::predef(HKEY_CURRENT_USER),
        RegKey::predef(HKEY_LOCAL_MACHINE),
    ] {
        for candidate in collect_windows_android_studio_paths_from_hive(&hive) {
            push_unique_path(&mut candidates, candidate);
        }
    }
    candidates
}

#[cfg(windows)]
fn collect_windows_android_studio_paths_from_hive(hive: &RegKey) -> Vec<PathBuf> {
    let mut candidates = Vec::new();

    if let Ok(android_studio_key) = hive.open_subkey("Software\\Android Studio")
        && let Some(path) = read_windows_string_value(&android_studio_key, "Path")
        && let Some(candidate) = normalize_windows_install_path_candidate(&path)
    {
        push_unique_path(&mut candidates, candidate);
    }

    for uninstall_path in [
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
        "Software\\WOW6432Node\\Microsoft\\Windows\\CurrentVersion\\Uninstall",
    ] {
        if let Ok(uninstall_root) = hive.open_subkey(uninstall_path) {
            for subkey_name in uninstall_root.enum_keys().flatten() {
                if let Ok(uninstall_entry) = uninstall_root.open_subkey(&subkey_name)
                    && read_windows_string_value(&uninstall_entry, "DisplayName")
                        .as_deref()
                        .is_some_and(|name| name.contains("Android Studio"))
                {
                    for value_name in ["InstallLocation", "DisplayIcon"] {
                        if let Some(value) = read_windows_string_value(&uninstall_entry, value_name)
                            && let Some(candidate) =
                                normalize_windows_install_path_candidate(&value)
                        {
                            push_unique_path(&mut candidates, candidate);
                        }
                    }
                }
            }
        }
    }

    candidates
}

#[cfg(windows)]
fn read_windows_string_value(key: &RegKey, value_name: &str) -> Option<String> {
    key.get_value::<String, _>(value_name)
        .ok()
        .filter(|value| !value.trim().is_empty())
}

#[cfg(windows)]
fn normalize_windows_install_path_candidate(raw_value: &str) -> Option<PathBuf> {
    let trimmed = raw_value.trim();
    if trimmed.is_empty() {
        return None;
    }

    let path = if let Some(quoted) = trimmed.strip_prefix('"') {
        let end_quote = quoted.find('"')?;
        PathBuf::from(&quoted[..end_quote])
    } else {
        PathBuf::from(trimmed.split(',').next().unwrap_or(trimmed).trim())
    };

    let file_name = path
        .file_name()
        .and_then(|value| value.to_str())
        .map(|value| value.to_ascii_lowercase());

    match file_name.as_deref() {
        Some("studio.exe") | Some("studio64.exe") => path.parent()?.parent().map(Path::to_path_buf),
        Some("bin") => path.parent().map(Path::to_path_buf),
        Some(_) if path.extension().is_some() => None,
        _ => Some(path),
    }
}

#[cfg(windows)]
fn push_unique_path(paths: &mut Vec<PathBuf>, candidate: PathBuf) {
    if !paths.iter().any(|path| path == &candidate) {
        paths.push(candidate);
    }
}

/// Check if the Git AI plugin is installed for a detected IDE
pub fn is_plugin_installed(detected: &DetectedIde) -> bool {
    // Support both the legacy extracted directory name and the Marketplace-installed
    // directory name that JetBrains/Android Studio writes on disk.
    ["git-ai-intellij", "Git AI"]
        .into_iter()
        .map(|dir_name| detected.plugins_dir.join(dir_name))
        .any(|plugin_dir| plugin_dir.exists())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn test_plugin_version_suffix_prefers_product_info_data_directory_name() {
        let version_suffix =
            plugin_version_suffix(Some("IntelliJIdea2026.1"), "IU", Some("261.22158.277"));
        assert_eq!(version_suffix, "IntelliJIdea2026.1");
    }

    #[test]
    fn test_plugin_version_suffix_falls_back_to_product_code_when_data_directory_name_missing() {
        let version_suffix = plugin_version_suffix(None, "IU", Some("252.27397.103"));
        assert_eq!(version_suffix, "IU2025.2");
    }

    #[test]
    fn test_plugins_dir_for_windows_keeps_jetbrains_parent_for_regular_ides() {
        let plugins_dir = plugins_dir_for_platform(
            JetBrainsPlatform::Windows,
            Path::new("home"),
            Some(Path::new("appdata")),
            Some("IntelliJIdea2026.1"),
            "IU",
            Some("261.22158.277"),
        );
        assert_eq!(
            plugins_dir,
            PathBuf::from("appdata")
                .join("JetBrains")
                .join("IntelliJIdea2026.1")
                .join("plugins")
        );
    }

    #[test]
    fn test_plugins_dir_for_windows_uses_google_parent_for_android_studio() {
        let plugins_dir = plugins_dir_for_platform(
            JetBrainsPlatform::Windows,
            Path::new("home"),
            Some(Path::new("appdata")),
            Some("AndroidStudio2025.3.3"),
            "AI",
            Some("253.31033.145"),
        );
        assert_eq!(
            plugins_dir,
            PathBuf::from("appdata")
                .join("Google")
                .join("AndroidStudio2025.3.3")
                .join("plugins")
        );
    }

    #[test]
    fn test_plugins_dir_for_macos_uses_google_parent_for_android_studio() {
        let plugins_dir = plugins_dir_for_platform(
            JetBrainsPlatform::Macos,
            Path::new("home"),
            None,
            Some("AndroidStudio2025.3.3"),
            "AI",
            Some("253.31033.145"),
        );
        assert_eq!(
            plugins_dir,
            PathBuf::from("home")
                .join("Library")
                .join("Application Support")
                .join("Google")
                .join("AndroidStudio2025.3.3")
                .join("plugins")
        );
    }

    #[test]
    fn test_plugins_dir_for_linux_uses_google_parent_for_android_studio_without_plugins_suffix() {
        let plugins_dir = plugins_dir_for_platform(
            JetBrainsPlatform::Linux,
            Path::new("home"),
            None,
            Some("AndroidStudio2025.3.3"),
            "AI",
            Some("253.31033.145"),
        );
        assert_eq!(
            plugins_dir,
            PathBuf::from("home")
                .join(".local")
                .join("share")
                .join("Google")
                .join("AndroidStudio2025.3.3")
        );
    }

    #[test]
    fn test_plugins_dir_for_linux_keeps_documented_jetbrains_plugins_root() {
        let plugins_dir = plugins_dir_for_platform(
            JetBrainsPlatform::Linux,
            Path::new("home"),
            None,
            Some("WebStorm2026.1"),
            "WS",
            Some("261.24980.77"),
        );
        assert_eq!(
            plugins_dir,
            PathBuf::from("home")
                .join(".local")
                .join("share")
                .join("JetBrains")
                .join("WebStorm2026.1")
        );
    }

    #[test]
    fn test_is_plugin_installed_detects_legacy_extracted_directory() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("git-ai-intellij")).unwrap();

        let detected = DetectedIde {
            ide: &JETBRAINS_IDES[0],
            install_path: PathBuf::from("install"),
            binary_path: PathBuf::from("binary"),
            build_number: Some("261.22158.277".to_string()),
            major_build: Some(261),
            plugins_dir: temp.path().to_path_buf(),
        };

        assert!(is_plugin_installed(&detected));
    }

    #[test]
    fn test_is_plugin_installed_detects_marketplace_directory_name() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(temp.path().join("Git AI")).unwrap();

        let detected = DetectedIde {
            ide: &JETBRAINS_IDES[0],
            install_path: PathBuf::from("install"),
            binary_path: PathBuf::from("binary"),
            build_number: Some("261.22158.277".to_string()),
            major_build: Some(261),
            plugins_dir: temp.path().to_path_buf(),
        };

        assert!(is_plugin_installed(&detected));
    }

    #[cfg(windows)]
    #[test]
    fn test_default_windows_android_studio_install_paths_cover_both_program_files_roots() {
        let program_dirs = vec![
            PathBuf::from(r"C:\Program Files"),
            PathBuf::from(r"C:\Program Files (x86)"),
        ];
        let candidates = default_windows_android_studio_install_paths(&program_dirs);
        assert_eq!(
            candidates,
            vec![
                PathBuf::from(r"C:\Program Files\Android\Android Studio"),
                PathBuf::from(r"C:\Program Files (x86)\Android\Android Studio"),
            ]
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_windows_install_path_candidate_accepts_install_root() {
        assert_eq!(
            normalize_windows_install_path_candidate(r"D:\software\as"),
            Some(PathBuf::from(r"D:\software\as"))
        );
    }

    #[cfg(windows)]
    #[test]
    fn test_normalize_windows_install_path_candidate_strips_bin_and_executable_suffixes() {
        assert_eq!(
            normalize_windows_install_path_candidate(r"D:\software\as\bin"),
            Some(PathBuf::from(r"D:\software\as"))
        );
        assert_eq!(
            normalize_windows_install_path_candidate(r#""D:\software\as\bin\studio64.exe",0"#),
            Some(PathBuf::from(r"D:\software\as"))
        );
        assert_eq!(
            normalize_windows_install_path_candidate(r"D:\software\as\bin\studio.exe"),
            Some(PathBuf::from(r"D:\software\as"))
        );
    }
}
