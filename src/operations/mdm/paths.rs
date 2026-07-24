use crate::error::GitAiError;
use std::path::{Path, PathBuf};

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

/// Get the absolute path to the currently running binary
pub fn get_current_binary_path() -> Result<PathBuf, GitAiError> {
    let path = std::env::current_exe()?;

    // Canonicalize to resolve any symlinks
    let canonical = path.canonicalize()?;

    Ok(clean_path(canonical))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serial_test::serial;

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
}
