use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::authorship::attribution_sink::FileAttribution;
use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::working_log::{Checkpoint, WorkingLogEntry};
use crate::daemon::checkpoint::is_ai_author_id;

/// Compute the content fingerprint for a single line.
///
/// This is a permanent API contract:
/// `hex(sha256(line with trailing "\n" and "\r" stripped))[:12]`.
///
/// Only newline characters are stripped. Changing normalization requires a
/// hook schema version bump.
pub fn fingerprint_line(line: &str) -> String {
    fingerprint_line_bytes(line.as_bytes())
}

/// Compute a content fingerprint without requiring UTF-8 input.
pub fn fingerprint_line_bytes(line: &[u8]) -> String {
    let stripped = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .or_else(|| line.strip_suffix(b"\r"))
        .unwrap_or(line);
    let digest = Sha256::digest(stripped);
    let mut fingerprint = String::with_capacity(12);
    for byte in &digest[..6] {
        let _ = write!(fingerprint, "{byte:02x}");
    }
    fingerprint
}

/// Build per-file fingerprint entries from the latest working-log checkpoint.
///
/// Fingerprints come from checkpoint blobs rather than committed files. A
/// human edit after the checkpoint therefore does not match downstream.
pub fn build_file_attributions(
    working_log_dir: &Path,
    checkpoints: &[Checkpoint],
) -> Vec<FileAttribution> {
    let latest_entries = resolve_latest_entries(checkpoints);
    let blobs_dir = working_log_dir.join("blobs");
    let mut results = Vec::new();

    for (file_path, (entry, checkpoint)) in latest_entries {
        let line_ranges = collect_ai_line_ranges(&entry.line_attributions);
        if line_ranges.is_empty() {
            continue;
        }

        let blob = std::fs::read(blobs_dir.join(&entry.blob_sha));
        let (fingerprints, fingerprints_complete) = match blob {
            Ok(content) => fingerprints_for_ranges(&content, &line_ranges),
            Err(_) => (Vec::new(), false),
        };

        results.push(FileAttribution {
            file: file_path,
            session_id: checkpoint
                .agent_id
                .as_ref()
                .map(|agent| agent.id.clone())
                .unwrap_or_default(),
            model: checkpoint
                .agent_id
                .as_ref()
                .map(|agent| agent.model.clone())
                .unwrap_or_default(),
            tool: checkpoint
                .agent_id
                .as_ref()
                .map(|agent| agent.tool.clone())
                .unwrap_or_default(),
            line_ranges: line_ranges
                .iter()
                .map(|&(start, end)| [start, end])
                .collect(),
            fingerprints,
            fingerprints_complete,
        });
    }

    results.sort_by(|left, right| left.file.cmp(&right.file));
    results
}

fn fingerprints_for_ranges(content: &[u8], ranges: &[(u32, u32)]) -> (Vec<String>, bool) {
    let lines: Vec<&[u8]> = content.split(|byte| *byte == b'\n').collect();
    let mut fingerprints = Vec::new();
    let mut complete = true;

    for &(start, end) in ranges {
        for line_number in start..=end {
            match lines.get((line_number - 1) as usize) {
                Some(line) => fingerprints.push(fingerprint_line_bytes(line)),
                None => complete = false,
            }
        }
    }

    (fingerprints, complete)
}

type LatestEntryMap<'a> = HashMap<String, (&'a WorkingLogEntry, &'a Checkpoint)>;

fn resolve_latest_entries(checkpoints: &[Checkpoint]) -> LatestEntryMap<'_> {
    let mut latest = HashMap::new();
    for checkpoint in checkpoints {
        for entry in &checkpoint.entries {
            latest.insert(entry.file.clone(), (entry, checkpoint));
        }
    }
    latest
}

fn collect_ai_line_ranges(line_attributions: &[LineAttribution]) -> Vec<(u32, u32)> {
    let mut ranges: Vec<_> = line_attributions
        .iter()
        .filter(|attribution| is_ai_author_id(&attribution.author_id))
        .map(|attribution| (attribution.start_line, attribution.end_line))
        .collect();
    ranges.sort_by_key(|range| range.0);
    ranges
}

/// Find a working log before or after post-commit archival.
pub fn find_working_log_dir(git_dir: &Path, parent_sha: &str) -> Option<PathBuf> {
    let working_logs = git_dir.join("ai").join("working_logs");
    [parent_sha.to_string(), format!("old-{parent_sha}")]
        .into_iter()
        .map(|name| working_logs.join(name))
        .find(|path| path.is_dir())
}

/// Read all checkpoints from a working log.
pub fn read_checkpoints(working_log_dir: &Path) -> Result<Vec<Checkpoint>, String> {
    let content = std::fs::read_to_string(working_log_dir.join("checkpoints.jsonl"))
        .map_err(|error| format!("failed to read checkpoints: {error}"))?;
    content
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            serde_json::from_str(line)
                .map_err(|error| format!("failed to parse checkpoint: {error}"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::working_log::{
        AgentId, CheckpointKind, CheckpointLineStats, WorkingLogEntry,
    };

    #[test]
    fn fingerprint_normalizes_only_line_endings() {
        let expected = fingerprint_line("hello");
        assert_eq!(fingerprint_line("hello\n"), expected);
        assert_eq!(fingerprint_line("hello\r\n"), expected);
        assert_ne!(fingerprint_line("hello "), expected);
    }

    #[test]
    fn fingerprint_is_stable_twelve_character_hex() {
        assert_eq!(fingerprint_line("hello"), "2cf24dba5fb0");
    }

    #[test]
    fn range_fingerprints_preserve_order_and_duplicates() {
        let (fingerprints, complete) =
            fingerprints_for_ranges(b"same\nmiddle\nsame\n", &[(1, 1), (3, 3)]);
        assert!(complete);
        assert_eq!(
            fingerprints,
            vec![fingerprint_line("same"), fingerprint_line("same")]
        );
    }

    #[test]
    fn missing_line_marks_fingerprints_incomplete() {
        let (fingerprints, complete) = fingerprints_for_ranges(b"one\n", &[(1, 3)]);
        assert!(!complete);
        assert_eq!(fingerprints.len(), 2);
    }

    #[test]
    fn builds_attributions_from_checkpoint_blob() {
        let directory = tempfile::tempdir().unwrap();
        let blobs = directory.path().join("blobs");
        std::fs::create_dir(&blobs).unwrap();
        std::fs::write(blobs.join("blob-sha"), b"human\nai one\nai two\n").unwrap();

        let checkpoint = Checkpoint {
            kind: CheckpointKind::AiAgent,
            diff: String::new(),
            author: "agent".to_string(),
            entries: vec![WorkingLogEntry::new(
                "src/example.rs".to_string(),
                "blob-sha".to_string(),
                Vec::new(),
                vec![LineAttribution::new(2, 3, "agent".to_string(), None)],
            )],
            timestamp: 1,
            agent_id: Some(AgentId {
                tool: "cursor".to_string(),
                id: "session-id".to_string(),
                model: "model-id".to_string(),
            }),
            agent_metadata: None,
            line_stats: CheckpointLineStats::default(),
            api_version: String::new(),
            git_ai_version: None,
            known_human_metadata: None,
            trace_id: None,
        };

        let result = build_file_attributions(directory.path(), &[checkpoint]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].file, "src/example.rs");
        assert_eq!(result[0].line_ranges, vec![[2, 3]]);
        assert_eq!(
            result[0].fingerprints,
            vec![fingerprint_line("ai one"), fingerprint_line("ai two")]
        );
        assert!(result[0].fingerprints_complete);
        assert_eq!(result[0].session_id, "session-id");
    }
}
