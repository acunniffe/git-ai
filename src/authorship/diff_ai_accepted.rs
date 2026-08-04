use std::collections::{BTreeMap, HashMap};

use crate::authorship::ignore::{build_ignore_matcher, should_ignore_file_with_matcher};
use crate::commands::blame::GitAiBlameOptions;
use crate::error::GitAiError;
use crate::git::repository::Repository;

#[derive(Debug, Default)]
pub struct DiffAiAcceptedStats {
    pub total_ai_accepted: u32,
    pub per_tool_model: BTreeMap<String, u32>,
    pub per_prompt: BTreeMap<String, u32>,
    pub added_lines_by_file: HashMap<String, Vec<u32>>,
}

pub fn diff_ai_accepted_stats(
    repo: &Repository,
    from_ref: &str,
    to_ref: &str,
    oldest_commit: Option<&str>,
    ignore_patterns: &[String],
) -> Result<DiffAiAcceptedStats, GitAiError> {
    let mut added_lines_by_file = repo.diff_added_lines(from_ref, to_ref, None)?;
    let ignore_matcher = build_ignore_matcher(ignore_patterns);
    added_lines_by_file
        .retain(|file_path, _| !should_ignore_file_with_matcher(file_path, &ignore_matcher));

    let mut stats = DiffAiAcceptedStats::default();

    for (file_path, lines) in &mut added_lines_by_file {
        if lines.is_empty() {
            continue;
        }

        lines.sort_unstable();
        lines.dedup();
        let line_ranges = lines_to_ranges(lines);

        if line_ranges.is_empty() {
            continue;
        }

        let mut options = GitAiBlameOptions::default();
        #[allow(clippy::field_reassign_with_default)]
        {
            options.oldest_commit = oldest_commit.map(|value| value.to_string());
            options.newest_commit = Some(to_ref.to_string());
            options.line_ranges = line_ranges;
            options.no_output = true;
            options.use_prompt_hashes_as_names = true;
        }

        let blame_result = repo.blame(file_path, &options);
        let (line_authors, prompt_records) = match blame_result {
            Ok(result) => result,
            Err(_) => continue,
        };

        let mut author_tool_map: HashMap<String, String> = HashMap::new();
        for (hash, record) in &prompt_records {
            let tool_model = format!("{}::{}", record.agent_id.tool, record.agent_id.model);
            author_tool_map.insert(hash.clone(), tool_model);
        }

        for line in lines.iter() {
            if let Some(author_hash) = line_authors.get(line)
                && prompt_records.contains_key(author_hash)
            {
                stats.total_ai_accepted += 1;
                *stats.per_prompt.entry(author_hash.clone()).or_insert(0) += 1;
                if let Some(tool_model) = author_tool_map.get(author_hash) {
                    *stats.per_tool_model.entry(tool_model.clone()).or_insert(0) += 1;
                }
            }
        }
    }

    stats.added_lines_by_file = added_lines_by_file;
    Ok(stats)
}

fn lines_to_ranges(lines: &[u32]) -> Vec<(u32, u32)> {
    if lines.is_empty() {
        return Vec::new();
    }

    let mut ranges = Vec::new();
    let mut start = lines[0];
    let mut end = lines[0];

    for &line in &lines[1..] {
        if line == end + 1 {
            end = line;
        } else {
            ranges.push((start, end));
            start = line;
            end = line;
        }
    }

    ranges.push((start, end));

    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_lines_to_ranges_empty() {
        let lines = vec![];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 0);
    }

    #[test]
    fn test_lines_to_ranges_single() {
        let lines = vec![5];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (5, 5));
    }

    #[test]
    fn test_lines_to_ranges_consecutive() {
        let lines = vec![1, 2, 3, 4, 5];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 1);
        assert_eq!(ranges[0], (1, 5));
    }

    #[test]
    fn test_lines_to_ranges_non_consecutive() {
        let lines = vec![1, 3, 5, 7];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 4);
        assert_eq!(ranges[0], (1, 1));
        assert_eq!(ranges[1], (3, 3));
        assert_eq!(ranges[2], (5, 5));
        assert_eq!(ranges[3], (7, 7));
    }

    #[test]
    fn test_lines_to_ranges_mixed() {
        let lines = vec![1, 2, 3, 5, 6, 10];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 3);
        assert_eq!(ranges[0], (1, 3));
        assert_eq!(ranges[1], (5, 6));
        assert_eq!(ranges[2], (10, 10));
    }

    #[test]
    fn test_lines_to_ranges_two_groups() {
        let lines = vec![1, 2, 3, 10, 11, 12];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (1, 3));
        assert_eq!(ranges[1], (10, 12));
    }

    #[test]
    fn test_lines_to_ranges_large_numbers() {
        let lines = vec![100, 101, 102, 200, 201];
        let ranges = lines_to_ranges(&lines);
        assert_eq!(ranges.len(), 2);
        assert_eq!(ranges[0], (100, 102));
        assert_eq!(ranges[1], (200, 201));
    }

    #[test]
    fn test_diff_ai_accepted_stats_default() {
        let stats = DiffAiAcceptedStats::default();
        assert_eq!(stats.total_ai_accepted, 0);
        assert_eq!(stats.per_tool_model.len(), 0);
        assert_eq!(stats.per_prompt.len(), 0);
        assert!(stats.added_lines_by_file.is_empty());
    }

    #[test]
    fn test_diff_ai_accepted_stats_debug() {
        let stats = DiffAiAcceptedStats {
            total_ai_accepted: 10,
            per_tool_model: BTreeMap::new(),
            per_prompt: BTreeMap::new(),
            added_lines_by_file: HashMap::new(),
        };
        let debug_str = format!("{:?}", stats);
        assert!(debug_str.contains("DiffAiAcceptedStats"));
        assert!(debug_str.contains("10"));
    }
}
