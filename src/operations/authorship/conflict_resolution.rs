use std::collections::{HashMap, HashSet};

use crate::model::authorship_log::LineRange;
use crate::model::authorship_log_serialization::AuthorshipLog;

fn normalize_line_ranges(ranges: &[LineRange]) -> Vec<LineRange> {
    let mut lines: Vec<u32> = ranges.iter().flat_map(LineRange::expand).collect();
    lines.sort_unstable();
    lines.dedup();
    LineRange::compress_lines(&lines)
}

fn subtract_line_ranges(ranges: &[LineRange], covered: &[LineRange]) -> Vec<LineRange> {
    let mut remaining = ranges.to_vec();
    for covered_range in covered {
        remaining = remaining
            .iter()
            .flat_map(|range| range.remove(covered_range))
            .collect();
        if remaining.is_empty() {
            break;
        }
    }
    normalize_line_ranges(&remaining)
}

fn line_coverage_by_file(log: &AuthorshipLog) -> HashMap<String, Vec<LineRange>> {
    let mut coverage: HashMap<String, Vec<LineRange>> = HashMap::new();
    for attestation in &log.attestations {
        let file_coverage = coverage.entry(attestation.file_path.clone()).or_default();
        for entry in &attestation.entries {
            file_coverage.extend(entry.line_ranges.clone());
        }
    }
    for ranges in coverage.values_mut() {
        *ranges = normalize_line_ranges(ranges);
    }
    coverage
}

fn attestation_metadata_key(hash: &str) -> &str {
    hash.split("::").next().unwrap_or(hash)
}

fn retain_referenced_metadata(log: &mut AuthorshipLog) {
    let mut prompt_keys = HashSet::new();
    let mut human_keys = HashSet::new();
    let mut session_keys = HashSet::new();

    for attestation in &log.attestations {
        for entry in &attestation.entries {
            let key = attestation_metadata_key(&entry.hash).to_string();
            if key.starts_with("h_") {
                human_keys.insert(key);
            } else if key.starts_with("s_") {
                session_keys.insert(key);
            } else {
                prompt_keys.insert(key);
            }
        }
    }

    log.metadata
        .prompts
        .retain(|key, _| prompt_keys.contains(key));
    log.metadata
        .humans
        .retain(|key, _| human_keys.contains(key));
    log.metadata
        .sessions
        .retain(|key, _| session_keys.contains(key));
}

fn filter_resolution_log_to_uncovered_lines(
    mut resolution_log: AuthorshipLog,
    shifted_log: &AuthorshipLog,
) -> AuthorshipLog {
    let shifted_coverage = line_coverage_by_file(shifted_log);

    for attestation in &mut resolution_log.attestations {
        let covered = shifted_coverage
            .get(&attestation.file_path)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        for entry in &mut attestation.entries {
            entry.line_ranges = subtract_line_ranges(&entry.line_ranges, covered);
        }
        attestation
            .entries
            .retain(|entry| !entry.line_ranges.is_empty());
    }

    resolution_log
        .attestations
        .retain(|attestation| !attestation.entries.is_empty());
    retain_referenced_metadata(&mut resolution_log);
    resolution_log
}

fn merge_file_attestations(target: &mut AuthorshipLog, source: &AuthorshipLog) {
    for source_attestation in &source.attestations {
        let target_attestation = target.get_or_create_file(&source_attestation.file_path);
        for source_entry in &source_attestation.entries {
            if let Some(target_entry) = target_attestation
                .entries
                .iter_mut()
                .find(|entry| entry.hash == source_entry.hash)
            {
                target_entry
                    .line_ranges
                    .extend(source_entry.line_ranges.clone());
                target_entry.line_ranges = normalize_line_ranges(&target_entry.line_ranges);
            } else {
                let mut entry = source_entry.clone();
                entry.line_ranges = normalize_line_ranges(&entry.line_ranges);
                target_attestation.entries.push(entry);
            }
        }
    }
}

pub fn merge_conflict_resolution_authorship(
    existing_shifted_log: Option<AuthorshipLog>,
    resolution_log: AuthorshipLog,
    commit_sha: &str,
) -> AuthorshipLog {
    let mut merged = existing_shifted_log.unwrap_or_default();
    let resolution_log = filter_resolution_log_to_uncovered_lines(resolution_log, &merged);

    merge_file_attestations(&mut merged, &resolution_log);
    merged.metadata.merge_missing_from(&resolution_log.metadata);
    merged.metadata.base_commit_sha = commit_sha.to_string();
    merged
}
