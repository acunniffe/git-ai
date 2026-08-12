#![allow(dead_code)]

use std::path::PathBuf;

use crate::repos::test_repo::TestRepo;
use serde_json::json;

pub fn isolated_bash_history_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated bash history db dir");
    let path = dir.path().join("bash-history.db");
    (dir, path.to_string_lossy().to_string())
}

pub fn codex_bash_hook_input(
    repo: &TestRepo,
    transcript_path: &std::path::Path,
    session_id: &str,
    tool_use_id: &str,
    hook_event_name: &str,
    command: &str,
) -> String {
    json!({
        "session_id": session_id,
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": hook_event_name,
        "tool_name": "Bash",
        "tool_use_id": tool_use_id,
        "tool_input": { "command": command },
        "transcript_path": transcript_path.to_string_lossy().to_string()
    })
    .to_string()
}

/// Get the path to a test fixture file
///
/// # Example
/// ```no_run
/// use test_utils::fixture_path;
///
/// let path = fixture_path("example.json");
/// // Returns: /path/to/project/tests/fixtures/example.json
/// ```
pub fn fixture_path(filename: &str) -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/")).join(filename)
}

/// Load the contents of a test fixture file as a string
///
/// # Example
/// ```no_run
/// use test_utils::load_fixture;
///
/// let contents = load_fixture("example.json");
/// // Returns the string contents of tests/fixtures/example.json
/// ```
///
/// # Panics
/// Panics if the fixture file cannot be read
pub fn load_fixture(filename: &str) -> String {
    std::fs::read_to_string(fixture_path(filename))
        .unwrap_or_else(|_| panic!("Failed to read fixture: {}", filename))
}
