#![allow(dead_code)]

use crate::repos::test_repo::TestRepo;
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Shared descriptive statistics for integration-test duration samples.
///
/// The helper retains the sorted samples so benchmark-specific wrappers can
/// preserve their existing percentile conventions and output schemas.
#[derive(Debug, Clone)]
pub struct DurationStatistics {
    sorted_samples: Vec<Duration>,
    average: Option<Duration>,
}

impl DurationStatistics {
    pub fn from_durations(durations: &[Duration]) -> Self {
        let mut sorted_samples = durations.to_vec();
        sorted_samples.sort();

        let average = (!sorted_samples.is_empty())
            .then(|| sorted_samples.iter().sum::<Duration>() / sorted_samples.len() as u32);

        Self {
            sorted_samples,
            average,
        }
    }

    pub fn count(&self) -> usize {
        self.sorted_samples.len()
    }

    pub fn sorted_samples(&self) -> &[Duration] {
        &self.sorted_samples
    }

    pub fn min(&self) -> Option<Duration> {
        self.sorted_samples.first().copied()
    }

    pub fn max(&self) -> Option<Duration> {
        self.sorted_samples.last().copied()
    }

    pub fn average(&self) -> Option<Duration> {
        self.average
    }

    /// Uses the one-indexed nearest-rank convention used by bash benchmarks.
    pub fn percentile_nearest_rank(&self, percentile: f64) -> Option<Duration> {
        self.assert_valid_percentile(percentile);

        let count = self.count();
        if count == 0 {
            return None;
        }

        let index = ((count as f64 * percentile).ceil() as usize)
            .saturating_sub(1)
            .min(count - 1);
        Some(self.sorted_samples[index])
    }

    /// Uses the zero-indexed upper-sample convention used by checkpoint benchmarks.
    pub fn percentile_upper_index(&self, percentile: f64) -> Option<Duration> {
        self.assert_valid_percentile(percentile);

        let count = self.count();
        if count == 0 {
            return None;
        }

        let index = ((count as f64 * percentile) as usize).min(count - 1);
        Some(self.sorted_samples[index])
    }

    pub fn std_dev_ms(&self) -> Option<f64> {
        let average = self.average?;
        let average_ms = average.as_secs_f64() * 1000.0;
        let variance = self
            .sorted_samples
            .iter()
            .map(|duration| {
                let duration_ms = duration.as_secs_f64() * 1000.0;
                (duration_ms - average_ms).powi(2)
            })
            .sum::<f64>()
            / self.count() as f64;

        Some(variance.sqrt())
    }

    fn assert_valid_percentile(&self, percentile: f64) {
        assert!(
            (0.0..=1.0).contains(&percentile),
            "percentile must be between 0.0 and 1.0, got {percentile}"
        );
    }
}

/// Builds the stable, valid Codex hook-input shapes used by integration tests.
///
/// Keep parser-boundary cases as raw JSON so they can exercise aliases,
/// malformed values, missing fields, and forward-compatible payloads directly.
#[derive(Serialize)]
pub struct CodexHookInput {
    session_id: String,
    cwd: String,
    hook_event_name: &'static str,
    tool_name: &'static str,
    tool_use_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    tool_input: CodexToolInput,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcript_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_response: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CodexToolInput {
    Patch { patch: String },
    Command { command: String },
}

impl CodexHookInput {
    /// Builds a valid pre-edit `apply_patch` hook input for `file_path`.
    pub fn pre_file_edit(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        file_path: impl AsRef<Path>,
    ) -> Self {
        Self::file_edit("PreToolUse", session_id, cwd, tool_use_id, file_path)
    }

    /// Builds a valid post-edit `apply_patch` hook input for `file_path`.
    pub fn post_file_edit(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        file_path: impl AsRef<Path>,
    ) -> Self {
        Self::file_edit("PostToolUse", session_id, cwd, tool_use_id, file_path)
    }

    /// Builds a valid pre-tool `Bash` hook input.
    pub fn pre_bash(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::bash("PreToolUse", "Bash", session_id, cwd, tool_use_id, command)
    }

    /// Builds a valid post-tool `Bash` hook input.
    pub fn post_bash(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::bash("PostToolUse", "Bash", session_id, cwd, tool_use_id, command)
    }

    /// Builds a valid pre-tool `exec_command` hook input.
    pub fn pre_exec_command(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::bash(
            "PreToolUse",
            "exec_command",
            session_id,
            cwd,
            tool_use_id,
            command,
        )
    }

    /// Builds a valid post-tool `exec_command` hook input.
    pub fn post_exec_command(
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::bash(
            "PostToolUse",
            "exec_command",
            session_id,
            cwd,
            tool_use_id,
            command,
        )
    }

    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    pub fn with_transcript_path(mut self, transcript_path: impl AsRef<Path>) -> Self {
        self.transcript_path = Some(transcript_path.as_ref().to_string_lossy().into_owned());
        self
    }

    pub fn with_tool_response(mut self, response: impl Into<String>) -> Self {
        self.tool_response = Some(response.into());
        self
    }

    /// Replaces an `apply_patch` payload while retaining the typed common fields.
    pub fn with_patch(mut self, patch: impl Into<String>) -> Self {
        self.tool_input = CodexToolInput::Patch {
            patch: patch.into(),
        };
        self
    }

    pub fn build(self) -> String {
        serde_json::to_string(&self).expect("Codex hook input should serialize")
    }

    fn file_edit(
        hook_event_name: &'static str,
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        file_path: impl AsRef<Path>,
    ) -> Self {
        let patch = format!(
            "*** Update File: {}\n",
            file_path.as_ref().to_string_lossy()
        );
        Self::new(
            hook_event_name,
            "apply_patch",
            session_id,
            cwd,
            tool_use_id,
            CodexToolInput::Patch { patch },
        )
    }

    fn bash(
        hook_event_name: &'static str,
        tool_name: &'static str,
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        command: impl Into<String>,
    ) -> Self {
        Self::new(
            hook_event_name,
            tool_name,
            session_id,
            cwd,
            tool_use_id,
            CodexToolInput::Command {
                command: command.into(),
            },
        )
    }

    fn new(
        hook_event_name: &'static str,
        tool_name: &'static str,
        session_id: impl Into<String>,
        cwd: impl AsRef<Path>,
        tool_use_id: impl Into<String>,
        tool_input: CodexToolInput,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            cwd: cwd.as_ref().to_string_lossy().into_owned(),
            hook_event_name,
            tool_name,
            tool_use_id: tool_use_id.into(),
            model: None,
            tool_input,
            transcript_path: None,
            tool_response: None,
        }
    }
}

/// Runs a normal Codex checkpoint built by [`CodexHookInput`].
pub fn checkpoint_codex(repo: &TestRepo, hook_input: CodexHookInput) {
    let hook_input = hook_input.build();
    repo.checkpoint_with_hook_input("codex", &hook_input)
        .expect("Codex checkpoint should succeed");
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

/// Read a JSONL fixture into its non-blank JSON values.
///
/// Malformed JSONL is surfaced as an [`std::io::ErrorKind::InvalidData`] error
/// so raw-event fidelity tests can distinguish invalid fixture data from I/O
/// failures.
pub fn read_jsonl_fixture(path: impl AsRef<Path>) -> std::io::Result<Vec<Value>> {
    let contents = std::fs::read_to_string(path)?;

    contents
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
        })
        .collect()
}

/// Get the path to a transcript fixture file under `tests/transcripts/fixtures/`.
/// Distinct root from [`fixture_path`] (`tests/fixtures/`) — do not merge.
pub fn transcript_fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("transcripts")
        .join("fixtures")
        .join(name)
}

/// Extract the outermost `{ ... }` JSON object from mixed CLI output by
/// locating the first `{` and the last `}`. Does not validate nesting; used
/// to strip surrounding log/diagnostic text before `serde_json::from_str`.
pub fn extract_json_object(output: &str) -> String {
    let start = output.find('{').unwrap_or(0);
    let end = output.rfind('}').unwrap_or(output.len().saturating_sub(1));
    output[start..=end].to_string()
}

/// Run a raw `git` command against `cwd` and assert it succeeds. Deliberately
/// unsynced/unisolated (no HOME override, no daemon sync) — different from
/// `TestRepo::git`, which wires up test isolation and daemon syncing.
pub fn raw_git(cwd: &Path, args: &[&str]) {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Same as [`raw_git`], but returns trimmed stdout instead of discarding it.
pub fn run_git_stdout(cwd: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("git command should run");
    assert!(
        output.status.success(),
        "git {:?} failed:\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// Create a fresh temp dir holding an isolated bash-history sqlite db path.
/// The returned `TempDir` must be kept alive for the duration of the test.
pub fn isolated_bash_history_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated bash history db dir");
    let path = dir.path().join("bash-history.db");
    (dir, path.to_string_lossy().to_string())
}
