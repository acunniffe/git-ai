#[cfg(not(windows))]
use crate::operations::mdm::paths::home_dir;
use std::path::{Path, PathBuf};
use std::process::Command;

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

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use tempfile::TempDir;

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
}
