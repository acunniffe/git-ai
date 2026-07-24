use crate::error::GitAiError;
use crate::model::imara_diff_utils::{LineChangeTag, compute_line_changes};
use std::fs;
use std::io::Write;
use std::path::Path;

/// Text-dedup constructor for the atomic-write filesystem errors below;
/// Display output is unchanged from the sites it replaces.
fn atomic_write_io_error(
    what: &str,
    location: impl std::fmt::Display,
    e: std::io::Error,
) -> GitAiError {
    GitAiError::Generic(format!("Failed to {} {}: {}", what, location, e))
}

/// Write data to a file atomically (write to temp, then rename)
/// If the path is a symlink, writes to the target file (preserving the symlink)
pub fn write_atomic(path: &Path, data: &[u8]) -> Result<(), GitAiError> {
    let target_path = if path.is_symlink() {
        fs::canonicalize(path)
            .map_err(|e| atomic_write_io_error("resolve symlink", path.display(), e))?
    } else {
        path.to_path_buf()
    };

    // Ensure parent directory exists before writing. This guards against
    // environments (e.g. nushell) where the parent may not yet exist when
    // write_atomic is reached. See #1039.
    ensure_parent_dir(&target_path)?;

    let tmp_path = target_path.with_extension("tmp");
    {
        let mut file = fs::File::create(&tmp_path)
            .map_err(|e| atomic_write_io_error("create temp file", tmp_path.display(), e))?;
        file.write_all(data)?;
        file.sync_all()?;
    }
    fs::rename(&tmp_path, &target_path).map_err(|e| {
        atomic_write_io_error(
            "rename",
            format!("{} to {}", tmp_path.display(), target_path.display()),
            e,
        )
    })?;
    Ok(())
}

/// Ensure parent directory exists
pub fn ensure_parent_dir(path: &Path) -> Result<(), GitAiError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|e| atomic_write_io_error("create directory", parent.display(), e))?;
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    #[cfg(unix)]
    use std::path::PathBuf;
    use tempfile::TempDir;

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
