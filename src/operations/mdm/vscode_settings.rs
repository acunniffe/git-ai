use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::EditorCliCommand;
use crate::operations::mdm::file_ops::{generate_diff, write_atomic};
use crate::operations::mdm::paths::home_dir;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
use std::fs;
use std::path::{Path, PathBuf};

/// Check if running in GitHub Codespaces environment
/// In Codespaces, VS Code extensions must be configured via devcontainer.json
/// rather than installed via CLI
pub fn is_github_codespaces() -> bool {
    std::env::var("CODESPACES")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Check if a settings target path should be processed
pub fn should_process_settings_target(path: &Path) -> bool {
    path.exists() || path.parent().map(|parent| parent.exists()).unwrap_or(false)
}

/// Get candidate paths for VS Code/Cursor settings
pub fn settings_path_candidates(product: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    #[cfg(windows)]
    {
        if let Ok(appdata) = std::env::var("APPDATA") {
            paths.push(
                PathBuf::from(&appdata)
                    .join(product)
                    .join("User")
                    .join("settings.json"),
            );
        }
        paths.push(
            home_dir()
                .join("AppData")
                .join("Roaming")
                .join(product)
                .join("User")
                .join("settings.json"),
        );
    }

    #[cfg(target_os = "macos")]
    {
        paths.push(
            home_dir()
                .join("Library")
                .join("Application Support")
                .join(product)
                .join("User")
                .join("settings.json"),
        );
    }

    #[cfg(all(unix, not(target_os = "macos")))]
    {
        paths.push(
            home_dir()
                .join(".config")
                .join(product)
                .join("User")
                .join("settings.json"),
        );
    }

    paths.sort();
    paths.dedup();
    paths
}

/// Get settings paths for multiple products
pub fn settings_paths_for_products(product_names: &[&str]) -> Vec<PathBuf> {
    let mut paths: Vec<PathBuf> = product_names
        .iter()
        .flat_map(|product| settings_path_candidates(product))
        .collect();

    paths.sort();
    paths.dedup();
    paths
}

/// Check if a VS Code extension is installed
pub fn is_vsc_editor_extension_installed(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
) -> Result<bool, GitAiError> {
    // NOTE: We try up to 3 times, because the editor CLI can be flaky (throws intermittent JS errors)
    let mut last_error_message: Option<String> = None;
    for attempt in 1..=3 {
        let cmd_result = cli.command(&["--list-extensions"]).output();

        match cmd_result {
            Ok(output) => {
                if !output.status.success() {
                    last_error_message = Some(String::from_utf8_lossy(&output.stderr).to_string());
                } else {
                    let stdout = String::from_utf8_lossy(&output.stdout);
                    return Ok(stdout.contains(id_or_vsix));
                }
            }
            Err(e) => {
                last_error_message = Some(e.to_string());
            }
        }
        if attempt < 3 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    Err(GitAiError::Generic(last_error_message.unwrap_or_else(
        || format!("{} CLI '--list-extensions' failed", cli.program),
    )))
}

/// Install a VS Code extension
pub fn install_vsc_editor_extension(
    cli: &EditorCliCommand,
    id_or_vsix: &str,
) -> Result<(), GitAiError> {
    // NOTE: We try up to 3 times, because the editor CLI can be flaky (throws intermittent JS errors)
    let mut last_error_message: Option<String> = None;
    for attempt in 1..=3 {
        let cmd_status = cli
            .command(&["--install-extension", id_or_vsix, "--force"])
            .status();

        match cmd_status {
            Ok(status) => {
                if status.success() {
                    return Ok(());
                }
                last_error_message = Some(format!("{} extension install failed", cli.program));
            }
            Err(e) => {
                last_error_message = Some(e.to_string());
            }
        }
        if attempt < 3 {
            std::thread::sleep(std::time::Duration::from_millis(300));
        }
    }
    Err(GitAiError::Generic(last_error_message.unwrap_or_else(
        || format!("{} extension install failed", cli.program),
    )))
}

/// Update VS Code chat hook settings in a settings.json/jsonc file.
///
/// Ensures `"chat.useHooks"` is set to `true`.
pub fn update_vscode_chat_hook_settings(
    settings_path: &Path,
    dry_run: bool,
) -> Result<Option<String>, GitAiError> {
    let original = if settings_path.exists() {
        fs::read_to_string(settings_path)?
    } else {
        String::new()
    };

    let parse_input = if original.trim().is_empty() {
        "{}".to_string()
    } else {
        original.clone()
    };

    let parse_options = ParseOptions::default();
    let root = CstRootNode::parse(&parse_input, &parse_options).map_err(|err| {
        GitAiError::Generic(format!(
            "Failed to parse {}: {}",
            settings_path.display(),
            err
        ))
    })?;

    let object = root.object_value_or_set();
    let mut changed = false;
    let mut enable_setting = |key: &str| match object.get(key) {
        Some(prop) => {
            let should_update = match prop.value() {
                Some(node) => match node.as_boolean_lit() {
                    Some(bool_node) => !bool_node.value(),
                    None => true,
                },
                None => true,
            };

            if should_update {
                prop.set_value(jsonc_parser::json!(true));
                changed = true;
            }
        }
        None => {
            object.append(key, jsonc_parser::json!(true));
            changed = true;
        }
    };

    enable_setting("chat.useHooks");
    enable_setting("github.copilot.chat.otel.dbSpanExporter.enabled");

    if !changed {
        return Ok(None);
    }

    let new_content = root.to_string();
    let diff_output = generate_diff(settings_path, &original, &new_content);

    if !dry_run {
        if let Some(parent) = settings_path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)?;
        }
        write_atomic(settings_path, new_content.as_bytes())?;
    }

    Ok(Some(diff_output))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_is_github_codespaces() {
        // Save original value
        let original = std::env::var("CODESPACES").ok();

        // SAFETY: This test modifies environment variables which is inherently
        // unsafe in multi-threaded contexts. This test should run in isolation.
        unsafe {
            // Test when CODESPACES is not set
            std::env::remove_var("CODESPACES");
            assert!(!is_github_codespaces());

            // Test when CODESPACES is set to "true"
            std::env::set_var("CODESPACES", "true");
            assert!(is_github_codespaces());

            // Test when CODESPACES is set to other values
            std::env::set_var("CODESPACES", "false");
            assert!(!is_github_codespaces());

            std::env::set_var("CODESPACES", "1");
            assert!(!is_github_codespaces());

            std::env::set_var("CODESPACES", "");
            assert!(!is_github_codespaces());

            // Restore original value
            match original {
                Some(val) => std::env::set_var("CODESPACES", val),
                None => std::env::remove_var("CODESPACES"),
            }
        }
    }

    #[test]
    fn test_update_vscode_chat_hook_settings_enables_use_hooks() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        let initial = r#"{
    // keep existing entries
    "chat.useHooks": false
}
"#;
        fs::write(&settings_path, initial).unwrap();

        let result = update_vscode_chat_hook_settings(&settings_path, false).unwrap();
        assert!(result.is_some());

        let final_content = fs::read_to_string(&settings_path).unwrap();
        assert!(final_content.contains("// keep existing entries"));
        assert!(final_content.contains("\"chat.useHooks\": true"));
        assert!(final_content.contains("otel.dbSpanExporter.enabled\": true"));
    }

    #[test]
    fn test_update_vscode_chat_hook_settings_detects_no_change() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        let initial = r#"{
    "chat.useHooks": true,
    "github.copilot.chat.otel.dbSpanExporter.enabled": true
}
"#;
        fs::write(&settings_path, initial).unwrap();

        let result = update_vscode_chat_hook_settings(&settings_path, false).unwrap();
        assert!(result.is_none());

        let final_content = fs::read_to_string(&settings_path).unwrap();
        assert_eq!(final_content, initial);
    }

    #[test]
    fn test_update_vscode_chat_hook_settings_adds_use_hooks_to_empty() {
        let temp_dir = TempDir::new().unwrap();
        let settings_path = temp_dir.path().join("settings.json");
        fs::write(&settings_path, "{}\n").unwrap();

        let result = update_vscode_chat_hook_settings(&settings_path, false).unwrap();
        assert!(result.is_some());

        let final_content = fs::read_to_string(&settings_path).unwrap();
        assert!(final_content.contains("\"chat.useHooks\": true"));
    }
}
