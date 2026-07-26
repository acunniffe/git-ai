use std::path::{Path, PathBuf};

pub const DEBUG_SELF_CHECK_REMOTE_URL: &str = "https://git-ai.invalid/debug/self-check.git";
pub const DEBUG_SELF_CHECK_NORMALIZED_REMOTE_URL: &str = "https://git-ai.invalid/debug/self-check";
pub const DEBUG_SELF_CHECK_DIR_NAME: &str = "debug-self-checks";

pub fn is_debug_self_check_remote_url(url: &str) -> bool {
    let trimmed = url.trim().trim_end_matches('/');
    trimmed == DEBUG_SELF_CHECK_REMOTE_URL
        || trimmed == DEBUG_SELF_CHECK_NORMALIZED_REMOTE_URL
        || crate::repo_url::normalize_repo_url(trimmed)
            .is_ok_and(|normalized| normalized == DEBUG_SELF_CHECK_NORMALIZED_REMOTE_URL)
}

pub fn debug_self_check_root() -> PathBuf {
    // Honors GIT_AI_INTERNAL_DIR (machine-local on a shared NFS home). Falls back
    // to the default ~/.git-ai/internal when the internal dir cannot be resolved,
    // preserving prior behavior.
    crate::config::internal_dir_path()
        .unwrap_or_else(|| {
            crate::mdm::utils::home_dir()
                .join(".git-ai")
                .join("internal")
        })
        .join(DEBUG_SELF_CHECK_DIR_NAME)
}

pub fn path_is_in_debug_self_check_root(path: &Path) -> bool {
    let root = debug_self_check_root();
    let root = std::fs::canonicalize(&root).unwrap_or(root);
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    path.starts_with(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_debug_self_check_remote_url_matches_raw_and_normalized() {
        assert!(is_debug_self_check_remote_url(
            "https://git-ai.invalid/debug/self-check.git"
        ));
        assert!(is_debug_self_check_remote_url(
            "https://git-ai.invalid/debug/self-check"
        ));
        assert!(!is_debug_self_check_remote_url(
            "https://example.com/debug/self-check.git"
        ));
    }
}
