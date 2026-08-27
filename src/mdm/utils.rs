use crate::authorship::imara_diff_utils::{LineChangeTag, compute_line_changes};
use crate::error::GitAiError;
use jsonc_parser::ParseOptions;
use jsonc_parser::cst::CstRootNode;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

// Minimum version requirements
pub const MIN_CURSOR_VERSION: (u32, u32) = (1, 7);
pub const MIN_CODE_VERSION: (u32, u32) = (1, 99);
pub const MIN_CLAUDE_VERSION: (u32, u32) = (2, 0);
pub const MIN_CODEX_VERSION: (u32, u32) = (0, 124);

/// Get version from a binary's --version output
pub fn get_binary_version(binary: &str) -> Result<String, GitAiError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| GitAiError::Generic(format!("Failed to run {} --version: {}", binary, e)))?;

    if !output.status.success() {
        return Err(GitAiError::Generic(format!(
            "{} --version failed with status: {}",
            binary, output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

/// Get version from an editor CLI command's --version output
pub fn get_editor_version(cli: &EditorCliCommand) -> Result<String, GitAiError> {
    let output = cli.command(&["--version"]).output().map_err(|e| {
        GitAiError::Generic(format!("Failed to run {} --version: {}", cli.program, e))
    })?;

    if !output.status.success() {
        return Err(GitAiError::Generic(format!(
            "{} --version failed with status: {}",
            cli.program, output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

/// Parse version string to extract major.minor version
/// Handles formats like "1.7.38", "1.104.3", "2.0.8 (Claude Code)"
pub fn parse_version(version_str: &str) -> Option<(u32, u32)> {
    for token in version_str.split_whitespace() {
        let version_part = token
            .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.')
            .trim_start_matches('v');

        let parts: Vec<&str> = version_part.split('.').collect();
        if parts.len() < 2 {
            continue;
        }

        let Ok(major) = parts[0].parse::<u32>() else {
            continue;
        };
        let Ok(minor) = parts[1].parse::<u32>() else {
            continue;
        };

        return Some((major, minor));
    }
    None
}

/// Compare version against minimum requirement
/// Returns true if version >= min_version
pub fn version_meets_requirement(version: (u32, u32), min_version: (u32, u32)) -> bool {
    if version.0 > min_version.0 {
        return true;
    }
    if version.0 == min_version.0 && version.1 >= min_version.1 {
        return true;
    }
    false
}

/// Check if a binary with the given name exists in the system PATH
pub fn binary_exists(name: &str) -> bool {
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in std::env::split_paths(&path_var) {
            // First check exact name as provided
            let candidate = dir.join(name);
            if candidate.exists() && candidate.is_file() {
                return true;
            }

            // On Windows, executables usually have extensions listed in PATHEXT
            #[cfg(windows)]
            {
                let pathext =
                    std::env::var("PATHEXT").unwrap_or_else(|_| ".EXE;.BAT;.CMD;.COM".to_string());
                for ext in pathext.split(';') {
                    let ext = ext.trim();
                    if ext.is_empty() {
                        continue;
                    }
                    let ext = if ext.starts_with('.') {
                        ext.to_string()
                    } else {
                        format!(".{}", ext)
                    };
                    let candidate = dir.join(format!("{}{}", name, ext));
                    if candidate.exists() && candidate.is_file() {
                        return true;
                    }
                }
            }
        }
    }
    false
}

/// Represents a resolved command for running an editor's CLI.
/// When the editor CLI (e.g. `code`, `cursor`) is in PATH, this wraps that simple command.
/// When the CLI is not in PATH, this wraps a fallback that calls Electron with `cli.js` directly,
/// mimicking what the shell script wrappers do.
pub struct EditorCliCommand {
    pub program: String,
    pub args_prefix: Vec<String>,
    pub env_vars: Vec<(String, String)>,
    /// Whether the program needs to be wrapped in `cmd /C` on Windows (for .cmd/.bat files)
    #[cfg(windows)]
    pub use_cmd_wrapper: bool,
}

impl EditorCliCommand {
    /// Create a command from a CLI binary found in PATH
    fn from_path(program: &str) -> Self {
        Self {
            program: program.to_string(),
            args_prefix: vec![],
            env_vars: vec![],
            #[cfg(windows)]
            use_cmd_wrapper: true,
        }
    }

    /// Create a command from an Electron binary and cli.js path
    fn from_cli_js(electron_path: &Path, cli_js_path: &Path) -> Self {
        Self {
            program: electron_path.to_string_lossy().to_string(),
            args_prefix: vec![cli_js_path.to_string_lossy().to_string()],
            env_vars: vec![("ELECTRON_RUN_AS_NODE".to_string(), "1".to_string())],
            #[cfg(windows)]
            use_cmd_wrapper: false,
        }
    }

    /// Build a std::process::Command with the given extra arguments
    pub fn command(&self, extra_args: &[&str]) -> Command {
        #[cfg(windows)]
        if self.use_cmd_wrapper {
            let mut cmd = Command::new("cmd");
            let mut args: Vec<&str> = vec!["/C", &self.program];
            args.extend(self.args_prefix.iter().map(|s| s.as_str()));
            args.extend(extra_args);
            cmd.args(&args);
            for (key, val) in &self.env_vars {
                cmd.env(key, val);
            }
            return cmd;
        }

        let mut cmd = Command::new(&self.program);
        for arg in &self.args_prefix {
            cmd.arg(arg);
        }
        cmd.args(extra_args);
        for (key, val) in &self.env_vars {
            cmd.env(key, val);
        }
        cmd
    }
}

/// Try to resolve the editor CLI command, first checking PATH, then falling back
/// to finding the Electron binary and `cli.js` directly in known install locations.
pub fn resolve_editor_cli(cli_name: &str) -> Option<EditorCliCommand> {
    if binary_exists(cli_name) {
        return Some(EditorCliCommand::from_path(cli_name));
    }

    find_editor_cli_js(cli_name)
}

/// Search known installation directories for the Electron binary and cli.js
fn find_editor_cli_js(cli_name: &str) -> Option<EditorCliCommand> {
    let candidates = get_editor_cli_candidates(cli_name);

    for (electron_path, cli_js_path) in candidates {
        if electron_path.is_file() && cli_js_path.is_file() {
            tracing::debug!(
                "{}: CLI not in PATH, using cli.js fallback at {}",
                cli_name,
                cli_js_path.display()
            );
            return Some(EditorCliCommand::from_cli_js(&electron_path, &cli_js_path));
        }
    }

    None
}

/// Return candidate (electron_binary, cli_js) paths for a given editor
fn get_editor_cli_candidates(cli_name: &str) -> Vec<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    #[cfg(not(windows))]
    let home = home_dir();

    match cli_name {
        "cursor" => {
            #[cfg(target_os = "macos")]
            {
                for apps_dir in [PathBuf::from("/Applications"), home.join("Applications")] {
                    let app = apps_dir.join("Cursor.app");
                    candidates.push((
                        app.join("Contents").join("MacOS").join("Cursor"),
                        app.join("Contents")
                            .join("Resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }

            #[cfg(all(unix, not(target_os = "macos")))]
            {
                for base in [
                    PathBuf::from("/opt/Cursor"),
                    PathBuf::from("/usr/share/cursor"),
                    home.join(".local").join("share").join("cursor"),
                    // Extracted AppImage location
                    home.join(".local").join("share").join("Cursor"),
                ] {
                    candidates.push((
                        base.join("cursor"),
                        base.join("resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }

            #[cfg(windows)]
            {
                if let Ok(localappdata) = std::env::var("LOCALAPPDATA") {
                    let base = PathBuf::from(&localappdata).join("Programs").join("Cursor");
                    candidates.push((
                        base.join("Cursor.exe"),
                        base.join("resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }
        }
        "windsurf" => {
            #[cfg(target_os = "macos")]
            {
                for apps_dir in [PathBuf::from("/Applications"), home.join("Applications")] {
                    let app = apps_dir.join("Windsurf.app");
                    candidates.push((
                        app.join("Contents").join("MacOS").join("Windsurf"),
                        app.join("Contents")
                            .join("Resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }
            #[cfg(all(unix, not(target_os = "macos")))]
            {
                for base in [
                    PathBuf::from("/opt/Windsurf"),
                    home.join(".local").join("share").join("windsurf"),
                    home.join(".local").join("share").join("Windsurf"),
                ] {
                    candidates.push((
                        base.join("windsurf"),
                        base.join("resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }
            #[cfg(windows)]
            {
                if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
                    let base = PathBuf::from(local_app_data)
                        .join("Programs")
                        .join("Windsurf");
                    candidates.push((
                        base.join("Windsurf.exe"),
                        base.join("resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }
        }
        "code" => {
            #[cfg(target_os = "macos")]
            {
                for apps_dir in [PathBuf::from("/Applications"), home.join("Applications")] {
                    for app_name in [
                        "Visual Studio Code.app",
                        "Visual Studio Code - Insiders.app",
                    ] {
                        let app = apps_dir.join(app_name);
                        candidates.push((
                            app.join("Contents").join("MacOS").join("Electron"),
                            app.join("Contents")
                                .join("Resources")
                                .join("app")
                                .join("out")
                                .join("cli.js"),
                        ));
                    }
                }
            }

            #[cfg(all(unix, not(target_os = "macos")))]
            {
                for base in [
                    PathBuf::from("/usr/share/code"),
                    PathBuf::from("/usr/lib/code"),
                    PathBuf::from("/opt/visual-studio-code"),
                    PathBuf::from("/usr/share/code-insiders"),
                    PathBuf::from("/snap/code/current/usr/share/code"),
                ] {
                    candidates.push((
                        base.join("code"),
                        base.join("resources")
                            .join("app")
                            .join("out")
                            .join("cli.js"),
                    ));
                }
            }

            #[cfg(windows)]
            {
                if let Ok(localappdata) = std::env::var("LOCALAPPDATA") {
                    for dir_name in ["Microsoft VS Code", "Microsoft VS Code Insiders"] {
                        let base = PathBuf::from(&localappdata).join("Programs").join(dir_name);
                        candidates.push((
                            base.join("Code.exe"),
                            base.join("resources")
                                .join("app")
                                .join("out")
                                .join("cli.js"),
                        ));
                    }
                }
            }
        }
        _ => {}
    }

    candidates
}

/// Check if running in GitHub Codespaces environment
/// In Codespaces, VS Code extensions must be configured via devcontainer.json
/// rather than installed via CLI
pub fn is_github_codespaces() -> bool {
    std::env::var("CODESPACES")
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// Get the user's home directory
pub fn home_dir() -> PathBuf {
    #[cfg(windows)]
    {
        if let Ok(userprofile) = std::env::var("USERPROFILE")
            && !userprofile.is_empty()
        {
            return PathBuf::from(userprofile);
        }

        if let (Ok(home_drive), Ok(home_path)) =
            (std::env::var("HOMEDRIVE"), std::env::var("HOMEPATH"))
            && !home_drive.is_empty()
            && !home_path.is_empty()
        {
            return PathBuf::from(format!("{}{}", home_drive, home_path));
        }

        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home);
        }

        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    }

    #[cfg(not(windows))]
    {
        if let Ok(home) = std::env::var("HOME")
            && !home.is_empty()
        {
            return PathBuf::from(home);
        }

        dirs::home_dir().unwrap_or_else(|| PathBuf::from("."))
    }
}

/// Claude config directory, respecting the CLAUDE_CONFIG_DIR env var.
/// Falls back to ~/.claude when unset.
pub fn claude_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home_dir().join(".claude")
}

/// Codex home directory, respecting the CODEX_HOME env var.
/// Falls back to ~/.codex when unset.
pub fn codex_home_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CODEX_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir);
    }
    home_dir().join(".codex")
}

/// Gemini CLI config directory, respecting the GEMINI_CLI_HOME env var.
/// GEMINI_CLI_HOME points to the user home root, and Gemini stores config under .gemini.
pub fn gemini_config_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GEMINI_CLI_HOME")
        && !dir.is_empty()
    {
        return PathBuf::from(dir).join(".gemini");
    }
    home_dir().join(".gemini")
}

/// Write data to a file atomically (write to temp, then rename)
/// If the path is a symlink, writes to the target file (preserving the symlink)
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), GitAiError> {
    let target_path = if path.is_symlink() {
        fs::canonicalize(path).map_err(|e| {
            GitAiError::Generic(format!(
                "Failed to resolve symlink {}: {}",
                path.display(),
                e
            ))
        })?
    } else {
        path.to_path_buf()
    };

    // Ensure parent directory exists before writing. This guards against
    // environments (e.g. nushell) where the parent may not yet exist when
    // write_atomic is reached. See #1039.
    ensure_parent_dir(&target_path)?;

    let tmp_path = target_path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path).map_err(|e| {
            GitAiError::Generic(format!(
                "Failed to create temp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, &target_path).map_err(|e| {
        GitAiError::Generic(format!(
            "Failed to rename {} to {}: {}",
            tmp_path.display(),
            target_path.display(),
            e
        ))
    })?;
    Ok(())
}

/// Ensure parent directory exists
pub fn ensure_parent_dir(path: &Path) -> Result<(), GitAiError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            GitAiError::Generic(format!(
                "Failed to create directory {}: {}",
                parent.display(),
                e
            ))
        })?;
    }
    Ok(())
}

/// Check if a command is a git-ai checkpoint command
pub fn is_git_ai_checkpoint_command(cmd: &str) -> bool {
    // Must contain "git-ai" and "checkpoint"
    cmd.contains("git-ai") && cmd.contains("checkpoint")
}

/// Generate a diff between old and new content
pub fn generate_diff(path: &Path, old_content: &str, new_content: &str) -> String {
    let changes = compute_line_changes(old_content, new_content);
    let mut diff_output = String::new();
    diff_output.push_str(&format!("--- {}\n", path.display()));
    diff_output.push_str(&format!("+++ {}\n", path.display()));

    for change in changes {
        let sign = match change.tag() {
            LineChangeTag::Delete => "-",
            LineChangeTag::Insert => "+",
            LineChangeTag::Equal => " ",
        };
        diff_output.push_str(&format!("{}{}", sign, change.value()));
    }

    diff_output
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

/// Strip the Windows extended-length path prefix (`\\?\`) if present.
/// On Windows, `std::fs::canonicalize` returns paths prefixed with `\\?\`
/// (e.g. `\\?\C:\Users\...`). This prefix causes problems when the path is
/// embedded in hook command strings for tools like Claude Code, Cursor, etc.
pub fn clean_path(path: PathBuf) -> PathBuf {
    let s = path.to_string_lossy();
    if let Some(stripped) = s.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    path
}

/// Normalize a Windows path to use forward slashes while preserving the drive letter.
/// e.g. `C:\Users\Administrator\.git-ai\bin\git-ai.exe` → `C:/Users/Administrator/.git-ai/bin/git-ai.exe`
/// Forward-slash paths work in both git bash and PowerShell on Windows.
/// Non-Windows paths (or paths that don't match `X:\...` pattern) are returned unchanged.
pub fn normalize_windows_path_for_shell(path: &Path) -> String {
    let s = path.to_string_lossy();
    let bytes = s.as_bytes();
    // Match a Windows absolute path like "C:\..." or "D:\..."
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        let drive_letter = (bytes[0] as char).to_ascii_uppercase();
        let rest = &s[2..]; // skip "C:"
        let rest_fwd = rest.replace('\\', "/");
        return format!("{}:{}", drive_letter, rest_fwd);
    }
    // Handle drive-relative path (e.g. C:foo)
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        let drive_letter = (bytes[0] as char).to_ascii_uppercase();
        let rest = &s[2..];
        let rest_fwd = rest.replace('\\', "/");
        return format!("{}:/{}", drive_letter, rest_fwd);
    }
    // For non-Windows paths, just return as-is
    s.into_owned()
}

pub fn shell_quote_path(path: &str) -> String {
    if path
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "-_./:=@".contains(character))
    {
        path.to_string()
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}

/// Get the absolute path to the currently running binary
pub fn get_current_binary_path() -> Result<PathBuf, GitAiError> {
    let path = std::env::current_exe()?;

    // Canonicalize to resolve any symlinks
    let canonical = path.canonicalize()?;

    Ok(clean_path(canonical))
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
    use serial_test::serial;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_parse_version() {
        // Test standard versions
        assert_eq!(parse_version("1.7.38"), Some((1, 7)));
        assert_eq!(parse_version("1.104.3"), Some((1, 104)));
        assert_eq!(parse_version("2.0.8"), Some((2, 0)));

        // Test version with extra text
        assert_eq!(parse_version("2.0.8 (Claude Code)"), Some((2, 0)));

        // Test edge cases
        assert_eq!(parse_version("1.0"), Some((1, 0)));
        assert_eq!(parse_version("10.20.30.40"), Some((10, 20)));

        // Test invalid versions
        assert_eq!(parse_version("1"), None);
        assert_eq!(parse_version("invalid"), None);
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn test_version_meets_requirement() {
        // Test exact match
        assert!(version_meets_requirement((1, 7), (1, 7)));

        // Test higher major version
        assert!(version_meets_requirement((2, 0), (1, 7)));

        // Test same major, higher minor
        assert!(version_meets_requirement((1, 8), (1, 7)));

        // Test lower major version
        assert!(!version_meets_requirement((0, 99), (1, 7)));

        // Test same major, lower minor
        assert!(!version_meets_requirement((1, 6), (1, 7)));

        // Test large numbers
        assert!(version_meets_requirement((1, 104), (1, 99)));
        assert!(!version_meets_requirement((1, 98), (1, 99)));
    }

    #[test]
    fn test_version_requirements() {
        // Test minimum version requirements against example versions from user

        // Cursor 1.7.38 should meet requirement of 1.7
        let cursor_version = parse_version("1.7.38").unwrap();
        assert!(version_meets_requirement(
            cursor_version,
            MIN_CURSOR_VERSION
        ));

        // Cursor 1.6.x should fail
        let old_cursor = parse_version("1.6.99").unwrap();
        assert!(!version_meets_requirement(old_cursor, MIN_CURSOR_VERSION));

        // VS Code 1.104.3 should meet requirement of 1.99
        let code_version = parse_version("1.104.3").unwrap();
        assert!(version_meets_requirement(code_version, MIN_CODE_VERSION));

        // VS Code 1.98.x should fail
        let old_code = parse_version("1.98.5").unwrap();
        assert!(!version_meets_requirement(old_code, MIN_CODE_VERSION));

        // Claude Code 2.0.8 should meet requirement of 2.0
        let claude_version = parse_version("2.0.8 (Claude Code)").unwrap();
        assert!(version_meets_requirement(
            claude_version,
            MIN_CLAUDE_VERSION
        ));

        // Claude Code 1.x should fail
        let old_claude = parse_version("1.9.9").unwrap();
        assert!(!version_meets_requirement(old_claude, MIN_CLAUDE_VERSION));
    }

    #[test]
    fn test_is_git_ai_checkpoint_command() {
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint claude --hook-input stdin"
        ));
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint claude"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint --hook-input"
        ));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint claude --hook-input \"$(cat)\""
        ));
        assert!(is_git_ai_checkpoint_command("git-ai checkpoint gemini"));
        assert!(is_git_ai_checkpoint_command(
            "git-ai checkpoint gemini --hook-input stdin"
        ));

        // Non-matching commands
        assert!(!is_git_ai_checkpoint_command("echo hello"));
        assert!(!is_git_ai_checkpoint_command("git status"));
        assert!(!is_git_ai_checkpoint_command("checkpoint"));
        assert!(!is_git_ai_checkpoint_command("git-ai"));
    }

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

    #[test]
    fn test_write_atomic_regular_file() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir.path().join("test.txt");

        write_atomic(&file_path, b"hello world").unwrap();

        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "hello world");
        assert!(!file_path.is_symlink());
    }

    #[test]
    #[cfg(unix)]
    fn test_write_atomic_preserves_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();

        // Create the actual target file in a subdirectory (simulating dotfiles)
        let target_dir = temp_dir.path().join("dotfiles");
        fs::create_dir_all(&target_dir).unwrap();
        let target_file = target_dir.join("settings.json");
        fs::write(&target_file, r#"{"original": true}"#).unwrap();

        // Create a symlink pointing to the target file
        let symlink_path = temp_dir.path().join("settings.json");
        symlink(&target_file, &symlink_path).unwrap();

        // Verify symlink is set up correctly
        assert!(symlink_path.is_symlink());
        assert_eq!(fs::read_link(&symlink_path).unwrap(), target_file);

        // Write through the symlink using write_atomic
        write_atomic(&symlink_path, b"updated content").unwrap();

        // The symlink should still exist and point to the same target
        assert!(symlink_path.is_symlink(), "symlink should be preserved");
        assert_eq!(
            fs::read_link(&symlink_path).unwrap(),
            target_file,
            "symlink target should be unchanged"
        );

        // The target file should have the new content
        let target_content = fs::read_to_string(&target_file).unwrap();
        assert_eq!(target_content, "updated content");

        // Reading through the symlink should also return the new content
        let symlink_content = fs::read_to_string(&symlink_path).unwrap();
        assert_eq!(symlink_content, "updated content");
    }

    #[test]
    #[cfg(unix)]
    fn test_write_atomic_preserves_relative_symlink() {
        use std::os::unix::fs::symlink;

        let temp_dir = TempDir::new().unwrap();

        // Create the actual target file in a subdirectory
        let target_dir = temp_dir.path().join("dotfiles").join("config");
        fs::create_dir_all(&target_dir).unwrap();
        let target_file = target_dir.join("settings.json");
        fs::write(&target_file, r#"{"original": true}"#).unwrap();

        // Create a directory for the symlink
        let link_dir = temp_dir.path().join(".config");
        fs::create_dir_all(&link_dir).unwrap();

        // Create a relative symlink
        let symlink_path = link_dir.join("settings.json");
        let relative_target = PathBuf::from("../dotfiles/config/settings.json");
        symlink(&relative_target, &symlink_path).unwrap();

        // Verify symlink is set up correctly
        assert!(symlink_path.is_symlink());

        // Write through the symlink using write_atomic
        write_atomic(&symlink_path, b"relative symlink content").unwrap();

        // The symlink should still exist
        assert!(symlink_path.is_symlink(), "symlink should be preserved");

        // The target file should have the new content
        let target_content = fs::read_to_string(&target_file).unwrap();
        assert_eq!(target_content, "relative symlink content");
    }

    #[test]
    fn test_editor_cli_command_from_path() {
        let cmd = EditorCliCommand::from_path("code");
        assert_eq!(cmd.program, "code");
        assert!(cmd.args_prefix.is_empty());
        assert!(cmd.env_vars.is_empty());
    }

    #[test]
    fn test_editor_cli_command_from_cli_js() {
        let electron = PathBuf::from("/Applications/Cursor.app/Contents/MacOS/Cursor");
        let cli_js = PathBuf::from("/Applications/Cursor.app/Contents/Resources/app/out/cli.js");
        let cmd = EditorCliCommand::from_cli_js(&electron, &cli_js);

        assert_eq!(cmd.program, electron.to_string_lossy());
        assert_eq!(cmd.args_prefix.len(), 1);
        assert_eq!(cmd.args_prefix[0], cli_js.to_string_lossy());
        assert_eq!(cmd.env_vars.len(), 1);
        assert_eq!(cmd.env_vars[0].0, "ELECTRON_RUN_AS_NODE");
        assert_eq!(cmd.env_vars[0].1, "1");
    }

    #[test]
    fn test_editor_cli_command_builds_command_with_args() {
        let cmd = EditorCliCommand::from_path("cursor");
        let built = cmd.command(&["--list-extensions"]);
        // On Windows, from_path uses cmd /C wrapper, so the program is "cmd"
        #[cfg(windows)]
        assert_eq!(built.get_program(), "cmd");
        #[cfg(not(windows))]
        assert_eq!(built.get_program(), "cursor");
    }

    #[test]
    fn test_editor_cli_command_from_cli_js_builds_command_with_env() {
        let electron = PathBuf::from("/usr/bin/electron");
        let cli_js = PathBuf::from("/usr/share/code/resources/app/out/cli.js");
        let cmd = EditorCliCommand::from_cli_js(&electron, &cli_js);
        let built = cmd.command(&["--version"]);

        assert_eq!(built.get_program(), "/usr/bin/electron");
        // Env should include ELECTRON_RUN_AS_NODE
        let envs: Vec<_> = built.get_envs().collect();
        assert!(envs.iter().any(|(k, v)| {
            k.to_string_lossy() == "ELECTRON_RUN_AS_NODE"
                && v.map(|v| v.to_string_lossy() == "1").unwrap_or(false)
        }));
    }

    #[test]
    fn test_resolve_editor_cli_returns_none_for_unknown() {
        // An unknown editor name should return None (no binary in PATH, no known install dirs)
        assert!(resolve_editor_cli("nonexistent-editor-xyz").is_none());
    }

    #[test]
    fn test_resolve_editor_cli_finds_cli_js_fallback() {
        // Create a fake editor installation directory structure (unix only)
        #[cfg(unix)]
        let temp_dir = TempDir::new().unwrap();
        #[cfg(unix)]
        let base = temp_dir.path().join("FakeEditor.app");

        #[cfg(target_os = "macos")]
        {
            let electron = base.join("Contents").join("MacOS").join("Cursor");
            let cli_js = base
                .join("Contents")
                .join("Resources")
                .join("app")
                .join("out")
                .join("cli.js");
            fs::create_dir_all(electron.parent().unwrap()).unwrap();
            fs::create_dir_all(cli_js.parent().unwrap()).unwrap();
            fs::write(&electron, "fake-electron").unwrap();
            fs::write(&cli_js, "fake-cli-js").unwrap();

            // The find_editor_cli_js function searches hardcoded paths,
            // so we can't easily test the full resolution. But we can test the
            // EditorCliCommand::from_cli_js path which is the actual fallback logic.
            let cmd = EditorCliCommand::from_cli_js(&electron, &cli_js);
            assert_eq!(cmd.program, electron.to_string_lossy());
            assert!(!cmd.args_prefix.is_empty());
            assert!(
                cmd.env_vars
                    .iter()
                    .any(|(k, _)| k == "ELECTRON_RUN_AS_NODE")
            );
        }

        #[cfg(all(unix, not(target_os = "macos")))]
        {
            let electron = base.join("cursor");
            let cli_js = base
                .join("resources")
                .join("app")
                .join("out")
                .join("cli.js");
            fs::create_dir_all(cli_js.parent().unwrap()).unwrap();
            fs::write(&electron, "fake-electron").unwrap();
            fs::write(&cli_js, "fake-cli-js").unwrap();

            let cmd = EditorCliCommand::from_cli_js(&electron, &cli_js);
            assert_eq!(cmd.program, electron.to_string_lossy());
            assert!(!cmd.args_prefix.is_empty());
            assert!(
                cmd.env_vars
                    .iter()
                    .any(|(k, _)| k == "ELECTRON_RUN_AS_NODE")
            );
        }
    }

    #[test]
    fn test_get_editor_cli_candidates_returns_expected_paths() {
        // Test that candidates are returned for known editors
        let cursor_candidates = get_editor_cli_candidates("cursor");
        assert!(
            !cursor_candidates.is_empty(),
            "cursor should have candidates"
        );

        let code_candidates = get_editor_cli_candidates("code");
        assert!(!code_candidates.is_empty(), "code should have candidates");

        // All candidate paths should end with expected file names
        for (electron, cli_js) in &cursor_candidates {
            assert!(
                cli_js.ends_with("cli.js"),
                "cli.js path should end with cli.js, got: {:?}",
                cli_js
            );
            let electron_name = electron.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                electron_name.contains("Cursor") || electron_name.contains("cursor"),
                "Electron binary for cursor should contain 'cursor' or 'Cursor', got: {}",
                electron_name
            );
        }

        for (electron, cli_js) in &code_candidates {
            assert!(
                cli_js.ends_with("cli.js"),
                "cli.js path should end with cli.js, got: {:?}",
                cli_js
            );
            let electron_name = electron.file_name().unwrap().to_string_lossy().to_string();
            assert!(
                electron_name.contains("Electron")
                    || electron_name.contains("code")
                    || electron_name.contains("Code"),
                "Electron binary for code should contain expected name, got: {}",
                electron_name
            );
        }

        // Unknown editor should return empty
        let unknown_candidates = get_editor_cli_candidates("unknown");
        assert!(unknown_candidates.is_empty());
    }

    #[test]
    fn test_normalize_windows_path_for_shell_converts_windows_path() {
        // Fixes #1413: use forward-slash Windows paths that work in both git bash AND PowerShell
        let path = PathBuf::from(r"C:\Users\Administrator\.git-ai\bin\git-ai.exe");
        let result = normalize_windows_path_for_shell(&path);
        assert_eq!(
            result, "C:/Users/Administrator/.git-ai/bin/git-ai.exe",
            "should convert Windows path to forward-slash format"
        );
    }

    #[test]
    fn test_normalize_windows_path_for_shell_converts_different_drive_letter() {
        let path = PathBuf::from(r"D:\Projects\code\app.exe");
        let result = normalize_windows_path_for_shell(&path);
        assert_eq!(
            result, "D:/Projects/code/app.exe",
            "should convert D: drive path to forward-slash format"
        );
    }

    #[test]
    fn test_normalize_windows_path_for_shell_preserves_unix_path() {
        let path = PathBuf::from("/usr/local/bin/git-ai");
        let result = normalize_windows_path_for_shell(&path);
        assert_eq!(
            result, "/usr/local/bin/git-ai",
            "should preserve unix paths unchanged"
        );
    }

    #[test]
    fn test_shell_quote_path_quotes_spaces_and_single_quotes() {
        assert_eq!(shell_quote_path("/tmp/profile root"), "'/tmp/profile root'");
        assert_eq!(
            shell_quote_path("/tmp/user's-profile"),
            "'/tmp/user'\\''s-profile'"
        );
        assert_eq!(shell_quote_path("/tmp/profile"), "/tmp/profile");
    }

    #[test]
    fn test_normalize_windows_path_for_shell_handles_extended_prefix_after_clean() {
        // After clean_path strips \\?\ prefix, the path looks like C:\...
        let raw = PathBuf::from(r"\\?\C:\Users\USERNAME\.git-ai\bin\git-ai.exe");
        let cleaned = clean_path(raw);
        let result = normalize_windows_path_for_shell(&cleaned);
        assert_eq!(
            result, "C:/Users/USERNAME/.git-ai/bin/git-ai.exe",
            "should convert cleaned Windows path to forward-slash format"
        );
    }

    #[test]
    fn test_normalize_windows_path_for_shell_handles_drive_relative_path() {
        // Drive-relative path like C:foo (no separator after colon)
        let path = PathBuf::from("C:foo");
        let result = normalize_windows_path_for_shell(&path);
        assert_eq!(
            result, "C:/foo",
            "should insert separator between drive letter and relative path"
        );
    }

    #[test]
    fn test_clean_path_strips_windows_prefix() {
        let path = PathBuf::from(r"\\?\C:\Users\test\.git-ai\bin\git-ai.exe");
        let cleaned = clean_path(path);
        let s = cleaned.to_string_lossy();
        assert!(
            !s.starts_with(r"\\?\"),
            "clean_path should strip the \\\\?\\ prefix, got: {}",
            s
        );
        assert!(
            s.contains("git-ai"),
            "clean_path should preserve the rest of the path, got: {}",
            s
        );
    }

    #[test]
    fn test_clean_path_preserves_normal_windows_path() {
        let path = PathBuf::from(r"C:\Users\test\.git-ai\bin\git-ai.exe");
        let cleaned = clean_path(path.clone());
        assert_eq!(cleaned, path);
    }

    #[test]
    fn test_clean_path_preserves_unix_path() {
        let path = PathBuf::from("/usr/local/bin/git-ai");
        let cleaned = clean_path(path.clone());
        assert_eq!(cleaned, path);
    }

    #[test]
    #[serial]
    fn test_claude_config_dir_defaults_to_home_dot_claude() {
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        let dir = claude_config_dir();
        assert_eq!(dir, home_dir().join(".claude"));
    }

    #[test]
    #[serial]
    fn test_claude_config_dir_respects_env_var() {
        let custom = "/tmp/my-claude-config";
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", custom);
        }
        let dir = claude_config_dir();
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        assert_eq!(dir, PathBuf::from(custom));
    }

    #[test]
    #[serial]
    fn test_claude_config_dir_ignores_empty_env_var() {
        unsafe {
            std::env::set_var("CLAUDE_CONFIG_DIR", "");
        }
        let dir = claude_config_dir();
        unsafe {
            std::env::remove_var("CLAUDE_CONFIG_DIR");
        }
        assert_eq!(dir, home_dir().join(".claude"));
    }

    #[test]
    #[serial]
    fn test_codex_home_dir_defaults_to_home_dot_codex() {
        let prev = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::remove_var("CODEX_HOME");
        }
        let dir = codex_home_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
        assert_eq!(dir, home_dir().join(".codex"));
    }

    #[test]
    #[serial]
    fn test_gemini_config_dir_defaults_to_home_dot_gemini() {
        let prev = std::env::var_os("GEMINI_CLI_HOME");
        unsafe {
            std::env::remove_var("GEMINI_CLI_HOME");
        }
        let dir = gemini_config_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("GEMINI_CLI_HOME", value),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
        assert_eq!(dir, home_dir().join(".gemini"));
    }

    #[test]
    #[serial]
    fn test_codex_home_dir_respects_env_var() {
        let prev = std::env::var_os("CODEX_HOME");
        let custom = "/tmp/my-codex-home";
        unsafe {
            std::env::set_var("CODEX_HOME", custom);
        }
        let dir = codex_home_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
        assert_eq!(dir, PathBuf::from(custom));
    }

    #[test]
    #[serial]
    fn test_gemini_config_dir_respects_env_var() {
        let prev = std::env::var_os("GEMINI_CLI_HOME");
        let custom = "/tmp/my-gemini-home";
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", custom);
        }
        let dir = gemini_config_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("GEMINI_CLI_HOME", value),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
        assert_eq!(dir, PathBuf::from(custom).join(".gemini"));
    }

    #[test]
    #[serial]
    fn test_codex_home_dir_ignores_empty_env_var() {
        let prev = std::env::var_os("CODEX_HOME");
        unsafe {
            std::env::set_var("CODEX_HOME", "");
        }
        let dir = codex_home_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("CODEX_HOME", value),
                None => std::env::remove_var("CODEX_HOME"),
            }
        }
        assert_eq!(dir, home_dir().join(".codex"));
    }

    #[test]
    #[serial]
    fn test_gemini_config_dir_ignores_empty_env_var() {
        let prev = std::env::var_os("GEMINI_CLI_HOME");
        unsafe {
            std::env::set_var("GEMINI_CLI_HOME", "");
        }
        let dir = gemini_config_dir();
        unsafe {
            match prev {
                Some(value) => std::env::set_var("GEMINI_CLI_HOME", value),
                None => std::env::remove_var("GEMINI_CLI_HOME"),
            }
        }
        assert_eq!(dir, home_dir().join(".gemini"));
    }

    /// Regression test for #1039: write_atomic should create parent directories
    /// if they do not exist, preventing "No such file or directory" errors.
    #[test]
    fn test_write_atomic_creates_parent_dirs() {
        let temp_dir = TempDir::new().unwrap();
        // Path whose parent directory does NOT yet exist
        let file_path = temp_dir
            .path()
            .join("nonexistent")
            .join("subdir")
            .join("test.json");
        assert!(!file_path.parent().unwrap().exists());

        write_atomic(&file_path, b"{\"key\": \"value\"}").unwrap();

        assert!(file_path.exists());
        let content = fs::read_to_string(&file_path).unwrap();
        assert_eq!(content, "{\"key\": \"value\"}");
    }

    /// Regression test for #1039: ensure_parent_dir handles nested missing dirs.
    #[test]
    fn test_ensure_parent_dir_creates_nested() {
        let temp_dir = TempDir::new().unwrap();
        let file_path = temp_dir
            .path()
            .join("a")
            .join("b")
            .join("c")
            .join("file.txt");
        assert!(!temp_dir.path().join("a").exists());

        ensure_parent_dir(&file_path).unwrap();

        assert!(file_path.parent().unwrap().exists());
    }

    /// Regression test for #1039: ensure_parent_dir is a no-op for root-level paths.
    #[test]
    fn test_ensure_parent_dir_no_parent() {
        // A path with no parent component should not error
        let path = Path::new("standalone_file.txt");
        ensure_parent_dir(path).unwrap();
    }
}
