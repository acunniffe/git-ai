use crate::model::working_log::AgentId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    pub username: String,
    pub email: String,
}

/// Represents either a single line or a range of lines
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum LineRange {
    Single(u32),
    Range(u32, u32), // start, end (inclusive)
}

impl LineRange {
    pub(crate) fn inclusive_bounds(&self) -> (u32, u32) {
        match self {
            Self::Single(line) => (*line, *line),
            Self::Range(start, end) => (*start, *end),
        }
    }

    pub fn contains(&self, line: u32) -> bool {
        match self {
            LineRange::Single(l) => *l == line,
            LineRange::Range(start, end) => line >= *start && line <= *end,
        }
    }

    #[allow(dead_code)]
    pub fn overlaps(&self, other: &LineRange) -> bool {
        match (self, other) {
            (LineRange::Single(l1), LineRange::Single(l2)) => l1 == l2,
            (LineRange::Single(l), LineRange::Range(start, end)) => *l >= *start && *l <= *end,
            (LineRange::Range(start, end), LineRange::Single(l)) => *l >= *start && *l <= *end,
            (LineRange::Range(start1, end1), LineRange::Range(start2, end2)) => {
                start1 <= end2 && start2 <= end1
            }
        }
    }

    /// Remove a line or range from this range, returning the remaining parts
    #[allow(dead_code)]
    pub fn remove(&self, to_remove: &LineRange) -> Vec<LineRange> {
        match (self, to_remove) {
            (LineRange::Single(l), LineRange::Single(r)) => {
                if l == r {
                    vec![]
                } else {
                    vec![self.clone()]
                }
            }
            (LineRange::Single(l), LineRange::Range(start, end)) => {
                if *l >= *start && *l <= *end {
                    vec![]
                } else {
                    vec![self.clone()]
                }
            }
            (LineRange::Range(start, end), LineRange::Single(r)) => {
                if *r < *start || *r > *end {
                    vec![self.clone()]
                } else if *r == *start && *r == *end {
                    vec![]
                } else if *r == *start {
                    vec![LineRange::Range(*start + 1, *end)]
                } else if *r == *end {
                    vec![LineRange::Range(*start, *end - 1)]
                } else {
                    vec![
                        LineRange::Range(*start, *r - 1),
                        LineRange::Range(*r + 1, *end),
                    ]
                }
            }
            (LineRange::Range(start1, end1), LineRange::Range(start2, end2)) => {
                if *start2 > *end1 || *end2 < *start1 {
                    // No overlap
                    vec![self.clone()]
                } else {
                    let mut result = Vec::new();
                    // Left part
                    if *start1 < *start2 {
                        result.push(LineRange::Range(*start1, *start2 - 1));
                    }
                    // Right part
                    if *end1 > *end2 {
                        result.push(LineRange::Range(*end2 + 1, *end1));
                    }
                    result
                }
            }
        }
    }

    /// Convert a sorted list of line numbers into compressed ranges
    pub fn compress_lines(lines: &[u32]) -> Vec<LineRange> {
        Self::compress_sorted_lines(lines, |start, end| {
            if start == end {
                Self::Single(start)
            } else {
                Self::Range(start, end)
            }
        })
    }

    /// Convert sorted line numbers into inclusive tuple bounds without sorting
    /// or deduplicating the input.
    pub(crate) fn compress_bounds(lines: &[u32]) -> Vec<(u32, u32)> {
        Self::compress_sorted_lines(lines, |start, end| (start, end))
    }

    fn compress_sorted_lines<T>(
        lines: &[u32],
        mut make_range: impl FnMut(u32, u32) -> T,
    ) -> Vec<T> {
        if lines.is_empty() {
            return vec![];
        }

        let mut ranges = Vec::new();
        let mut current_start = lines[0];
        let mut current_end = lines[0];

        for &line in &lines[1..] {
            if line == current_end + 1 {
                current_end = line;
            } else {
                ranges.push(make_range(current_start, current_end));
                current_start = line;
                current_end = line;
            }
        }

        ranges.push(make_range(current_start, current_end));

        ranges
    }

    #[allow(dead_code)]
    pub fn expand(&self) -> Vec<u32> {
        match self {
            LineRange::Single(l) => vec![*l],
            LineRange::Range(start, end) => (*start..=*end).collect(),
        }
    }

    /// Shift line numbers by a given offset
    /// - For insertions: offset is positive (shift lines down/forward)
    /// - For deletions: offset is negative (shift lines up/backward)
    /// - insertion_point: the line number where the change occurred
    #[allow(dead_code)]
    pub fn shift(&self, insertion_point: u32, offset: i32) -> Option<LineRange> {
        // Helper: apply offset to a line number, returning None if result is negative
        let apply_offset = |line: u32| -> Option<u32> {
            if line >= insertion_point {
                let shifted = (line as i64) + (offset as i64);
                if shifted >= 0 && shifted <= u32::MAX as i64 {
                    Some(shifted as u32)
                } else {
                    None
                }
            } else {
                Some(line)
            }
        };

        match self {
            LineRange::Single(l) => {
                let new_line = apply_offset(*l)?;
                Some(LineRange::Single(new_line))
            }
            LineRange::Range(start, end) => {
                let new_start = apply_offset(*start)?;
                let new_end = apply_offset(*end)?;

                // Ensure the range is still valid
                if new_start <= new_end {
                    if new_start == new_end {
                        Some(LineRange::Single(new_start))
                    } else {
                        Some(LineRange::Range(new_start, new_end))
                    }
                } else {
                    None
                }
            }
        }
    }
}

impl fmt::Display for LineRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LineRange::Single(l) => write!(f, "{}", l),
            LineRange::Range(start, end) => write!(f, "[{}, {}]", start, end),
        }
    }
}

/// Identity record for a known human author attested by an IDE extension
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HumanRecord {
    /// Git committer identity: "Alice Smith <alice@example.com>"
    pub author: String,
}

/// Prompt session details stored in the top-level prompts map keyed by short hash (agent_id + tool)
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PromptRecord {
    pub agent_id: AgentId,
    pub human_author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub messages_url: Option<String>,
    #[serde(default)]
    pub total_additions: u32,
    #[serde(default)]
    pub total_deletions: u32,
    #[serde(default)]
    pub accepted_lines: u32,
    #[serde(default)]
    pub overriden_lines: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_attributes: Option<HashMap<String, String>>,
}

/// Session record for lightweight session tracking without stats
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionRecord {
    pub agent_id: AgentId,
    pub human_author: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_attributes: Option<HashMap<String, String>>,
}

impl SessionRecord {
    /// Convert to a PromptRecord (with zeroed stats) for backwards-compatible lookup
    pub fn to_prompt_record(&self) -> PromptRecord {
        PromptRecord {
            agent_id: self.agent_id.clone(),
            human_author: self.human_author.clone(),
            messages_url: None,
            total_additions: 0,
            total_deletions: 0,
            accepted_lines: 0,
            overriden_lines: 0,
            custom_attributes: self.custom_attributes.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_range_inclusive_bounds_preserve_stored_values() {
        assert_eq!(LineRange::Single(7).inclusive_bounds(), (7, 7));
        assert_eq!(LineRange::Range(3, 9).inclusive_bounds(), (3, 9));
        assert_eq!(LineRange::Range(9, 3).inclusive_bounds(), (9, 3));
    }

    #[test]
    fn line_range_compression_primitives_stay_in_parity() {
        let cases = [
            (Vec::new(), Vec::new()),
            (vec![4], vec![LineRange::Single(4)]),
            (vec![2, 3, 4], vec![LineRange::Range(2, 4)]),
            (
                vec![1, 3, 5],
                vec![
                    LineRange::Single(1),
                    LineRange::Single(3),
                    LineRange::Single(5),
                ],
            ),
            (
                vec![1, 2, 4, 7, 8, 9],
                vec![
                    LineRange::Range(1, 2),
                    LineRange::Single(4),
                    LineRange::Range(7, 9),
                ],
            ),
        ];

        for (lines, expected) in cases {
            assert_eq!(LineRange::compress_lines(&lines), expected);
            assert_eq!(
                LineRange::compress_bounds(&lines),
                expected
                    .iter()
                    .map(LineRange::inclusive_bounds)
                    .collect::<Vec<_>>()
            );
        }
    }

    // --- LineRange::shift regression tests ---

    #[test]
    fn test_shift_single_underflow_returns_none() {
        // Single(5) at insertion_point=3 with offset=-10: 5 >= 3, so shifted = 5 + (-10) = -5 => None
        let result = LineRange::Single(5).shift(3, -10);
        assert_eq!(result, None);
    }

    #[test]
    fn test_shift_range_zero_offset_identity() {
        // Zero offset should be the identity transform
        let result = LineRange::Range(10, 20).shift(5, 0);
        assert_eq!(result, Some(LineRange::Range(10, 20)));
    }

    #[test]
    fn test_shift_range_partial_underflow() {
        // Range(2, 10) at insertion_point=0, offset=-5:
        //   start: 2 >= 0, so 2 + (-5) = -3 => None (apply_offset fails on start)
        let result = LineRange::Range(2, 10).shift(0, -5);
        assert_eq!(result, None);
    }

    #[test]
    fn test_shift_range_collapses_to_single() {
        // Range(10, 11) at insertion_point=11, offset=-1:
        //   start: 10 < 11, so stays 10
        //   end:   11 >= 11, so 11 + (-1) = 10
        //   10 == 10 => collapses to Single(10)
        let result = LineRange::Range(10, 11).shift(11, -1);
        assert_eq!(result, Some(LineRange::Single(10)));
    }

    #[test]
    fn test_shift_single_below_insertion_unchanged() {
        // Single(3) with insertion_point=5: 3 < 5, so line is unchanged
        let result = LineRange::Single(3).shift(5, 10);
        assert_eq!(result, Some(LineRange::Single(3)));
    }

    #[test]
    fn test_shift_single_large_value_i64_arithmetic() {
        // Single(u32::MAX) at insertion_point=0, offset=1:
        //   u32::MAX >= 0, so shifted = (u32::MAX as i64) + 1 = 4294967296
        //   shifted >= 0, so Some(4294967296 as u32) which wraps to 0
        //   This verifies the i64 arithmetic path doesn't panic.
        let result = LineRange::Single(u32::MAX).shift(0, 1);
        assert_eq!(
            result, None,
            "u32::MAX + 1 should overflow u32 and return None"
        );
    }
}
