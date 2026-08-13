#![allow(dead_code)]

use std::path::PathBuf;

use crate::repos::test_file::ExpectedLineExt;
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

pub fn setup_codex_bash_repo(
    initial_commit_message: &str,
) -> (tempfile::TempDir, TestRepo, PathBuf) {
    let (db_dir, _db_value, repo, transcript) =
        setup_codex_bash_repo_with_db_path(initial_commit_message);
    (db_dir, repo, transcript)
}

pub fn setup_codex_bash_repo_with_db_path(
    initial_commit_message: &str,
) -> (tempfile::TempDir, String, TestRepo, PathBuf) {
    let (db_dir, db_value) = isolated_bash_history_db_path();
    let repo = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
        db_value.as_str(),
    )]);
    std::fs::write(repo.path().join("base.txt"), "base\n").unwrap();
    repo.stage_all_and_commit(initial_commit_message).unwrap();
    repo.filename("base.txt")
        .assert_committed_lines(lines!["base".unattributed_human()]);
    let transcript = repo.path().join("codex-transcript.jsonl");
    std::fs::copy(fixture_path("codex-session-simple.jsonl"), &transcript).unwrap();
    (db_dir, db_value, repo, transcript)
}

pub fn checkpoint_codex_bash_hook(
    repo: &TestRepo,
    transcript_path: &std::path::Path,
    session_id: &str,
    tool_use_id: &str,
    event: &str,
    command: &str,
) {
    let input = codex_bash_hook_input(
        repo,
        transcript_path,
        session_id,
        tool_use_id,
        event,
        command,
    );
    repo.git_ai(&["checkpoint", "codex", "--hook-input", &input])
        .unwrap();
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
