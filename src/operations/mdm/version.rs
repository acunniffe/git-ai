use crate::error::GitAiError;
use crate::operations::mdm::editor_cli::EditorCliCommand;
use std::process::Command;

// Minimum version requirements
pub const MIN_CURSOR_VERSION: (u32, u32) = (1, 7);
pub const MIN_CODE_VERSION: (u32, u32) = (1, 99);
pub const MIN_CLAUDE_VERSION: (u32, u32) = (2, 0);

/// Extract the trimmed stdout of a completed `--version` invocation, or an
/// error using `program` for the failure message. Shared tail of
/// `get_binary_version` and `get_editor_version`.
fn extract_trimmed_version(
    program: &str,
    output: std::process::Output,
) -> Result<String, GitAiError> {
    if !output.status.success() {
        return Err(GitAiError::Generic(format!(
            "{} --version failed with status: {}",
            program, output.status
        )));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.trim().to_string())
}

/// Get version from a binary's --version output
pub fn get_binary_version(binary: &str) -> Result<String, GitAiError> {
    let output = Command::new(binary)
        .arg("--version")
        .output()
        .map_err(|e| GitAiError::Generic(format!("Failed to run {} --version: {}", binary, e)))?;

    extract_trimmed_version(binary, output)
}

/// Get version from an editor CLI command's --version output
pub fn get_editor_version(cli: &EditorCliCommand) -> Result<String, GitAiError> {
    let output = cli.command(&["--version"]).output().map_err(|e| {
        GitAiError::Generic(format!("Failed to run {} --version: {}", cli.program, e))
    })?;

    extract_trimmed_version(&cli.program, output)
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
