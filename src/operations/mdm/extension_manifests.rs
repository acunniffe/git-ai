//! Detection of VS Code-family extensions from installed-extension manifests.
//!
//! Each VS Code-family editor records installed extensions in a single
//! `extensions/extensions.json` under the user's home, so detection reads a
//! fixed set of manifest files — bounded by construction, no directory walks.

use crate::operations::mdm::paths::home_dir;
use std::fs;
use std::path::{Path, PathBuf};

const EXTENSION_MANIFEST_PATHS: &[&str] = &[
    ".vscode/extensions/extensions.json",
    ".vscode-server/extensions/extensions.json",
    ".cursor/extensions/extensions.json",
    ".windsurf/extensions/extensions.json",
];

/// Returns true when any editor's extension manifest lists `extension_id`.
pub(crate) fn any_manifest_lists_extension(extension_id: &str) -> bool {
    manifest_paths()
        .iter()
        .any(|path| manifest_lists_extension(path, extension_id))
}

fn manifest_paths() -> Vec<PathBuf> {
    let home = home_dir();
    EXTENSION_MANIFEST_PATHS
        .iter()
        .map(|path| home.join(path))
        .collect()
}

fn manifest_lists_extension(path: &Path, extension_id: &str) -> bool {
    let Ok(content) = fs::read_to_string(path) else {
        return false;
    };
    let Ok(extensions) = serde_json::from_str::<serde_json::Value>(&content) else {
        return false;
    };

    extensions.as_array().is_some_and(|extensions| {
        extensions.iter().any(|extension| {
            extension
                .get("identifier")
                .and_then(|identifier| identifier.get("id"))
                .and_then(|id| id.as_str())
                == Some(extension_id)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(content: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("extensions.json");
        fs::write(&path, content).unwrap();
        (dir, path)
    }

    #[test]
    fn test_manifest_lists_extension_matches_identifier_id() {
        let (_dir, path) = write_manifest(
            r#"[
  {
    "identifier": { "id": "saoudrizwan.claude-dev", "uuid": "test-uuid" },
    "version": "4.0.11",
    "relativeLocation": "saoudrizwan.claude-dev-4.0.11"
  }
]"#,
        );

        assert!(manifest_lists_extension(&path, "saoudrizwan.claude-dev"));
    }

    #[test]
    fn test_manifest_lists_extension_ignores_other_fields() {
        // The id must come from identifier.id — a matching relativeLocation
        // for a different extension must not count as installed.
        let (_dir, path) = write_manifest(
            r#"[
  {
    "identifier": { "id": "git-ai.git-ai-vscode" },
    "relativeLocation": "saoudrizwan.claude-dev-4.0.11"
  }
]"#,
        );

        assert!(!manifest_lists_extension(&path, "saoudrizwan.claude-dev"));
    }

    #[test]
    fn test_manifest_lists_extension_handles_missing_and_malformed_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let missing = dir.path().join("nope.json");
        assert!(!manifest_lists_extension(
            &missing,
            "saoudrizwan.claude-dev"
        ));

        let (_dir, malformed) = write_manifest("not json at all");
        assert!(!manifest_lists_extension(
            &malformed,
            "saoudrizwan.claude-dev"
        ));

        let (_dir, non_array) = write_manifest(r#"{"identifier":{"id":"saoudrizwan.claude-dev"}}"#);
        assert!(!manifest_lists_extension(
            &non_array,
            "saoudrizwan.claude-dev"
        ));
    }

    #[test]
    fn test_manifest_paths_cover_the_four_editor_layouts() {
        let paths = manifest_paths();
        assert_eq!(paths.len(), 4);
        for (path, expected) in paths.iter().zip(EXTENSION_MANIFEST_PATHS) {
            assert!(
                path.ends_with(expected),
                "{} should end with {expected}",
                path.display()
            );
        }
    }
}
