//! Install manifest: records every artifact written during installation so that
//! `git-ai uninstall` can reverse them exactly.
//!
//! Schema version: `1`.  The format is intentionally simple: a flat JSON object
//! with a `version` field and arrays for each artifact category.  All fields are
//! additive — older readers silently ignore unknown keys.
//!
//! Written to `~/.git-ai/install-manifest.json` by:
//! - `install.sh` / `install.ps1` (binary + symlinks + rc edits)
//! - `git-ai install-hooks` (git config keys + agent hooks, merged on top)

use crate::error::GitAiError;
use crate::operations::mdm::paths::home_dir;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Fence markers written around git-ai PATH lines in shell rc files.
/// `uninstall` removes the block between these markers (inclusive).
pub const FENCE_OPEN: &str = "# >>> git-ai >>>";
pub const FENCE_CLOSE: &str = "# <<< git-ai <<<";

/// Global git config keys that `install-hooks` writes and `uninstall` reverts.
pub const TRACE2_GIT_CONFIG_KEYS: &[&str] = &["trace2.eventTarget", "trace2.eventNesting"];

/// Schema version for the manifest JSON.
const MANIFEST_VERSION: u32 = 1;

/// Describes one rc file edited during installation.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct RcEntry {
    /// Absolute path to the rc file.
    pub path: String,
}

/// Top-level install manifest.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InstallManifest {
    /// Schema version — always `1` for this generation.
    pub version: u32,

    /// Absolute path to the installed git-ai binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary_path: Option<String>,

    /// Symlinks created (e.g. `~/.local/bin/git-ai`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub symlinks: Vec<String>,

    /// Shell rc files that were edited with a fence block.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rc_files: Vec<RcEntry>,

    /// Global git config keys written (e.g. `trace2.eventTarget`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub git_config_keys: Vec<String>,

    /// Agent hook IDs installed (mirrors `InstallStatus::Installed` tool ids).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub agent_hooks: Vec<String>,
}

impl InstallManifest {
    fn manifest_path() -> PathBuf {
        home_dir().join(".git-ai").join("install-manifest.json")
    }

    /// Load the manifest from disk, returning a default if absent or unreadable.
    pub fn load() -> Self {
        let path = Self::manifest_path();
        let Ok(bytes) = std::fs::read(&path) else {
            return Self::default_with_version();
        };
        serde_json::from_slice(&bytes).unwrap_or_else(|_| Self::default_with_version())
    }

    /// Persist the manifest, creating `~/.git-ai/` if needed.
    pub fn save(&self) -> Result<(), GitAiError> {
        let path = Self::manifest_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        std::fs::write(&path, json)?;
        Ok(())
    }

    fn default_with_version() -> Self {
        Self {
            version: MANIFEST_VERSION,
            ..Default::default()
        }
    }

    /// Record that `path` was edited with a fence block, deduplicating.
    pub fn add_rc_file(&mut self, path: &str) {
        if !self.rc_files.iter().any(|e| e.path == path) {
            self.rc_files.push(RcEntry {
                path: path.to_string(),
            });
        }
    }

    /// Record a git config key, deduplicating.
    pub fn add_git_config_key(&mut self, key: &str) {
        if !self.git_config_keys.iter().any(|k| k == key) {
            self.git_config_keys.push(key.to_string());
        }
    }

    /// Record an agent hook id, deduplicating.
    pub fn add_agent_hook(&mut self, id: &str) {
        if !self.agent_hooks.iter().any(|k| k == id) {
            self.agent_hooks.push(id.to_string());
        }
    }

    /// Load the manifest, record the given git-config keys and the agent hook
    /// IDs that are present in `statuses` (values "installed" or
    /// "already_installed"), then save.  Called by `install-hooks` after a
    /// successful run.  Accepting the string-valued status map avoids a
    /// circular dependency on `InstallStatus` in install_hooks.rs.
    pub fn record_install_hooks(
        git_config_keys: &[&str],
        statuses: &std::collections::HashMap<String, String>,
    ) -> Result<(), GitAiError> {
        let mut mf = Self::load();
        for k in git_config_keys {
            mf.add_git_config_key(k);
        }
        for (id, status) in statuses {
            if matches!(status.as_str(), "installed" | "already_installed") {
                mf.add_agent_hook(id);
            }
        }
        mf.save()
    }
}

// ─── Fence block helpers ────────────────────────────────────────────────────

/// Wrap `content` in a fence block.
///
/// Returns the complete block text including open/close markers and a trailing
/// newline so it can be appended directly to a rc file.
pub fn make_fence_block(content: &str) -> String {
    format!(
        "{}\n{}\n{}\n",
        FENCE_OPEN,
        content.trim_end_matches('\n'),
        FENCE_CLOSE
    )
}

/// Remove the fenced block from `text` (including the open/close lines).
///
/// Handles both LF and CRLF line endings: fence markers are matched after
/// stripping a trailing `\r`, and the original line endings are preserved.
/// When BOTH fence markers are absent, the file is returned unchanged (no
/// truncation on unbalanced/missing markers).  Duplicate fence blocks are
/// removed in a loop until none remain.
///
/// If no fence is found, attempts best-effort removal of any line that looks
/// like a legacy unfenced installer line (`# Added by git-ai installer …` or
/// any line containing `.git-ai/bin`).
pub fn remove_fence_block(text: &str) -> FenceRemovalResult {
    // Split on '\n', preserving the raw content of each segment (including a
    // trailing '\r' on CRLF lines).  Rejoining with '\n' restores the original.
    let lines: Vec<&str> = text.split('\n').collect();

    // Check whether both fence markers are present (CRLF-aware: strip '\r').
    let has_open = lines.iter().any(|l| l.trim_end_matches('\r') == FENCE_OPEN);
    let has_close = lines
        .iter()
        .any(|l| l.trim_end_matches('\r') == FENCE_CLOSE);

    if has_open && has_close {
        // Remove all fence blocks in a loop (parity with the shell/PS scripts
        // that also remove duplicate blocks in a single pass).
        let mut current: Vec<&str> = lines.clone();
        let mut removed_any = false;
        loop {
            let open_idx = current
                .iter()
                .position(|l| l.trim_end_matches('\r') == FENCE_OPEN);
            let close_idx = current
                .iter()
                .position(|l| l.trim_end_matches('\r') == FENCE_CLOSE);
            match (open_idx, close_idx) {
                (Some(oi), Some(ci)) if oi < ci => {
                    current.drain(oi..=ci);
                    removed_any = true;
                }
                _ => break,
            }
        }
        if removed_any {
            return FenceRemovalResult {
                text: current.join("\n"),
                removed_fence: true,
            };
        }
    } else if has_open || has_close {
        // Unbalanced markers — return the file unchanged to avoid truncation.
        return FenceRemovalResult {
            text: text.to_string(),
            removed_fence: false,
        };
    }

    // No fence — fall back to stripping individual legacy lines.
    let legacy_marker = "# Added by git-ai installer";
    let mut any_removed = false;
    let cleaned: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|l| {
            let bare = l.trim_end_matches('\r');
            let drop = bare.starts_with(legacy_marker) || bare.contains("/.git-ai/bin");
            if drop {
                any_removed = true;
            }
            !drop
        })
        .collect();

    if any_removed {
        FenceRemovalResult {
            text: cleaned.join("\n"),
            removed_fence: false,
        }
    } else {
        FenceRemovalResult {
            text: text.to_string(),
            removed_fence: false,
        }
    }
}

/// Result of [`remove_fence_block`].
pub struct FenceRemovalResult {
    /// The cleaned file text.
    pub text: String,
    /// `true` when a proper fence block was found and removed.
    pub removed_fence: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── manifest serialisation ──────────────────────────────────────────────

    #[test]
    fn manifest_roundtrips_through_json() {
        let mut m = InstallManifest {
            version: 1,
            binary_path: Some("/home/user/.git-ai/bin/git-ai".to_string()),
            ..Default::default()
        };
        m.add_rc_file("/home/user/.zshrc");
        m.add_git_config_key("trace2.eventTarget");
        m.add_agent_hook("claude-code");

        let json = serde_json::to_string_pretty(&m).unwrap();
        let loaded: InstallManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.version, 1);
        assert_eq!(
            loaded.binary_path.as_deref(),
            Some("/home/user/.git-ai/bin/git-ai")
        );
        assert_eq!(loaded.rc_files.len(), 1);
        assert_eq!(loaded.rc_files[0].path, "/home/user/.zshrc");
        assert_eq!(loaded.git_config_keys, vec!["trace2.eventTarget"]);
        assert_eq!(loaded.agent_hooks, vec!["claude-code"]);
    }

    #[test]
    fn add_rc_file_deduplicates() {
        let mut m = InstallManifest::default_with_version();
        m.add_rc_file("/home/user/.zshrc");
        m.add_rc_file("/home/user/.zshrc");
        assert_eq!(m.rc_files.len(), 1);
    }

    #[test]
    fn add_git_config_key_deduplicates() {
        let mut m = InstallManifest::default_with_version();
        m.add_git_config_key("trace2.eventTarget");
        m.add_git_config_key("trace2.eventTarget");
        assert_eq!(m.git_config_keys.len(), 1);
    }

    #[test]
    fn default_manifest_has_version_1() {
        let m = InstallManifest::default_with_version();
        assert_eq!(m.version, 1);
    }

    // ── fence block helpers ─────────────────────────────────────────────────

    #[test]
    fn make_fence_block_wraps_content() {
        let block = make_fence_block("export PATH=\"/foo:$PATH\"");
        assert!(block.starts_with(FENCE_OPEN));
        assert!(block.contains("export PATH"));
        assert!(block.trim_end().ends_with(FENCE_CLOSE));
    }

    #[test]
    fn remove_fence_block_exact_round_trip() {
        let original = "export FOO=1\n";
        let block = make_fence_block("export PATH=\"$HOME/.git-ai/bin:$PATH\"");
        let with_block = format!("{}{}", original, block);
        let result = remove_fence_block(&with_block);
        assert!(result.removed_fence, "should remove the fence");
        assert_eq!(result.text, original);
    }

    #[test]
    fn remove_fence_block_preserves_content_before_and_after() {
        let before = "export A=1\n";
        let after = "export B=2\n";
        let block = make_fence_block("export PATH=\"$HOME/.git-ai/bin:$PATH\"");
        let content = format!("{}{}{}", before, block, after);
        let result = remove_fence_block(&content);
        assert!(result.removed_fence);
        assert_eq!(result.text, format!("{}{}", before, after));
    }

    #[test]
    fn remove_fence_block_idempotent_on_clean_file() {
        let content = "export FOO=1\nexport BAR=2\n";
        let result = remove_fence_block(content);
        assert!(!result.removed_fence);
        assert_eq!(result.text, content);
    }

    #[test]
    fn remove_fence_block_strips_legacy_unfenced_lines() {
        let content = "export FOO=1\n# Added by git-ai installer on Mon Jan 1\nexport PATH=\"/x/.git-ai/bin:$PATH\"\nalias ll='ls -l'\n";
        let result = remove_fence_block(content);
        assert!(!result.removed_fence, "no fence block present");
        assert!(!result.text.contains(".git-ai"), "legacy lines removed");
        assert!(result.text.contains("export FOO=1"));
        assert!(result.text.contains("alias ll"));
    }

    #[test]
    fn remove_fence_block_noop_when_nothing_to_remove() {
        let content = "export FOO=1\n";
        let result = remove_fence_block(content);
        assert!(!result.removed_fence);
        assert_eq!(result.text, content);
    }

    #[test]
    fn remove_fence_block_removes_duplicate_blocks() {
        let block = make_fence_block("export PATH=\"$HOME/.git-ai/bin:$PATH\"");
        let content = format!("pre\n{}{}", block, block);
        let result = remove_fence_block(&content);
        assert!(result.removed_fence, "fence must be removed");
        assert!(
            !result.text.contains(FENCE_OPEN),
            "no fence marker should remain"
        );
        assert!(
            !result.text.contains(FENCE_CLOSE),
            "no fence marker should remain"
        );
        assert!(result.text.contains("pre"), "surrounding content preserved");
    }

    #[test]
    fn remove_fence_block_crlf_round_trip() {
        // CRLF file with a fence block: removal must preserve CRLF endings and
        // must not rewrite a file that contains no git-ai content.
        let crlf_content = "export FOO=1\r\n# >>> git-ai >>>\r\nexport PATH=\"$HOME/.git-ai/bin:$PATH\"\r\n# <<< git-ai <<<\r\nexport BAR=2\r\n";
        let result = remove_fence_block(crlf_content);
        assert!(result.removed_fence, "fence must be removed from CRLF file");
        assert_eq!(
            result.text, "export FOO=1\r\nexport BAR=2\r\n",
            "CRLF line endings must be preserved after fence removal"
        );
    }

    #[test]
    fn remove_fence_block_crlf_no_fence_unchanged() {
        // A CRLF file without any git-ai content must be returned byte-identical.
        let crlf_content = "export FOO=1\r\nexport BAR=2\r\n";
        let result = remove_fence_block(crlf_content);
        assert!(!result.removed_fence);
        assert_eq!(
            result.text, crlf_content,
            "CRLF file with no git-ai content must be byte-identical (no LF conversion)"
        );
    }

    #[test]
    fn remove_fence_block_open_without_close_is_noop() {
        // Unbalanced open marker: must not truncate the file.
        let content = format!(
            "export FOO=1\n{}\nexport PATH=...\nexport BAR=2\n",
            FENCE_OPEN
        );
        let result = remove_fence_block(&content);
        assert!(!result.removed_fence);
        assert_eq!(
            result.text, content,
            "file with only open marker must be unchanged"
        );
    }

    #[test]
    fn remove_fence_block_close_without_open_is_noop() {
        // Unbalanced close marker: must not truncate the file.
        let content = format!(
            "export FOO=1\nexport PATH=...\n{}\nexport BAR=2\n",
            FENCE_CLOSE
        );
        let result = remove_fence_block(&content);
        assert!(!result.removed_fence);
        assert_eq!(
            result.text, content,
            "file with only close marker must be unchanged"
        );
    }

    #[test]
    fn manifest_load_returns_default_when_file_absent() {
        // No home override needed — just call load() and ensure it does not panic.
        // (In the test environment there may or may not be a manifest on disk;
        //  we only assert it returns *something* without panicking.)
        let m = InstallManifest::load();
        assert_eq!(m.version, 1);
    }
}
