#![allow(dead_code)]

use crate::repos::test_repo::TestRepo;
use git_ai::metrics::MetricEvent;
use git_ai::metrics::attrs::attr_pos;
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::types::{MetricEventId, SparseArray};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

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

/// Extract the outermost JSON object from command output, ignoring any leading or
/// trailing non-JSON lines (for example daemon log noise on stderr).
pub fn extract_json_object(output: &str) -> String {
    let start = output.find('{').unwrap_or(0);
    let end = output.rfind('}').unwrap_or(output.len().saturating_sub(1));
    output[start..=end].to_string()
}

/// Create a temporary directory holding an isolated metrics database.
///
/// The returned `TempDir` must be kept alive for the lifetime of the test, since
/// dropping it deletes the database on disk.
pub fn isolated_metrics_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated metrics db dir");
    let path = dir.path().join("metrics.db");
    (dir, path.to_string_lossy().to_string())
}

/// A codex pre/post edit hook checkpoint for `file_path` (the mock presets are
/// excluded from commit metrics, so metric tests need a real preset).
pub fn codex_checkpoint(
    repo: &TestRepo,
    file_path: &Path,
    session_id: &str,
    hook_event_name: &str,
    tool_use_id: &str,
) {
    let hook_input = json!({
        "session_id": session_id,
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": hook_event_name,
        "tool_name": "apply_patch",
        "tool_use_id": tool_use_id,
        "model": "gpt-5",
        "tool_input": {
            "patch": format!("*** Update File: {}\n", file_path.to_string_lossy())
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
        .expect("codex checkpoint should succeed");
}

pub fn sparse_str(values: &SparseArray, pos: usize) -> Option<&str> {
    values
        .get(&pos.to_string())
        .and_then(|value| value.as_str())
}

pub fn sparse_u64(values: &SparseArray, pos: usize) -> Option<u64> {
    values
        .get(&pos.to_string())
        .and_then(|value| value.as_u64())
}

/// Every persisted `Committed` metric for `commit_sha` right now.
pub fn committed_metrics_for_commit(db_path: &str, commit_sha: &str) -> Vec<MetricEvent> {
    let db = MetricsDatabase::open_at_path(Path::new(db_path))
        .expect("metrics db should open at isolated path");
    db.get_metric_history(0, None, &[MetricEventId::Committed as u16])
        .expect("metric history should load")
        .into_iter()
        .filter(|record| sparse_str(&record.event.attrs, attr_pos::COMMIT_SHA) == Some(commit_sha))
        .map(|record| record.event)
        .collect()
}

/// Waits for the `Committed` metric of `commit_sha` to be persisted.
pub fn committed_metric_for_commit(db_path: &str, commit_sha: &str) -> MetricEvent {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(event) = committed_metrics_for_commit(db_path, commit_sha).pop() {
            return event;
        }
        if Instant::now() >= deadline {
            panic!("committed metric for {commit_sha} was not persisted");
        }
        std::thread::sleep(Duration::from_millis(50));
    }
}
