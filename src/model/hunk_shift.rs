use crate::model::attribution::LineAttribution;
use crate::model::authorship_log::LineRange;
use crate::model::authorship_log_serialization::{AttestationEntry, FileAttestation};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffHunk {
    pub old_start: u32,
    pub old_count: u32,
    pub new_start: u32,
    pub new_count: u32,
}

pub fn parse_range_spec(spec: &str) -> Option<(u32, u32)> {
    if let Some((start_str, count_str)) = spec.split_once(',') {
        let start = start_str.parse().ok()?;
        let count = count_str.parse().ok()?;
        Some((start, count))
    } else {
        let start = spec.parse().ok()?;
        Some((start, 1))
    }
}

pub fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let parts: Vec<&str> = line.split_whitespace().collect();
    if parts.len() < 4 || parts[0] != "@@" {
        return None;
    }

    let old_part = parts[1].trim_start_matches('-');
    let new_part = parts[2].trim_start_matches('+');

    let (old_start, old_count) = parse_range_spec(old_part)?;
    let (new_start, new_count) = parse_range_spec(new_part)?;

    Some(DiffHunk {
        old_start,
        old_count,
        new_start,
        new_count,
    })
}

/// (seg_start, seg_end, offset) — old line range [seg_start, seg_end] inclusive maps to
/// new line = old_line + offset.
fn build_preserved_segments(hunks: &[DiffHunk]) -> Vec<(u32, u32, i64)> {
    let mut segments: Vec<(u32, u32, i64)> = Vec::with_capacity(hunks.len() + 1);
    let mut offset: i64 = 0;
    let mut prev_old_end: u32 = 1;

    for hunk in hunks {
        if prev_old_end < hunk.old_start + 1 {
            // For pure insertions (old_count=0), old_start points to the line AFTER which
            // insertion happens, so lines up to and including old_start are preserved.
            let seg_end = if hunk.old_count == 0 {
                hunk.old_start
            } else {
                hunk.old_start.saturating_sub(1)
            };
            if prev_old_end <= seg_end {
                segments.push((prev_old_end, seg_end, offset));
            }
        }

        offset += hunk.new_count as i64 - hunk.old_count as i64;

        if hunk.old_count == 0 {
            prev_old_end = hunk.old_start + 1;
        } else {
            prev_old_end = hunk.old_start + hunk.old_count;
        }
    }

    segments.push((prev_old_end, u32::MAX, offset));
    segments
}

pub fn apply_hunk_shifts_to_attestation_entries(
    entries: &[AttestationEntry],
    hunks: &[DiffHunk],
) -> Vec<AttestationEntry> {
    if hunks.is_empty() {
        return entries.to_vec();
    }

    let segments = build_preserved_segments(hunks);

    let mut result: Vec<AttestationEntry> = Vec::with_capacity(entries.len());

    for entry in entries {
        let mut new_ranges: Vec<LineRange> = Vec::new();

        for range in &entry.line_ranges {
            let (range_start, range_end) = range.inclusive_bounds();

            for &(seg_start, seg_end, seg_offset) in &segments {
                let overlap_start = range_start.max(seg_start);
                let overlap_end = range_end.min(seg_end);

                if overlap_start <= overlap_end {
                    let new_start = (overlap_start as i64 + seg_offset).max(1) as u32;
                    let new_end = (overlap_end as i64 + seg_offset).max(1) as u32;

                    if new_start == new_end {
                        new_ranges.push(LineRange::Single(new_start));
                    } else {
                        new_ranges.push(LineRange::Range(new_start, new_end));
                    }
                }
            }
        }

        if !new_ranges.is_empty() {
            result.push(AttestationEntry {
                hash: entry.hash.clone(),
                line_ranges: new_ranges,
            });
        }
    }

    result
}

pub fn apply_hunk_shifts_to_file_attestation(
    file: &FileAttestation,
    hunks: &[DiffHunk],
) -> Option<FileAttestation> {
    let entries = apply_hunk_shifts_to_attestation_entries(&file.entries, hunks);
    if entries.is_empty() {
        None
    } else {
        Some(FileAttestation {
            file_path: file.file_path.clone(),
            entries,
        })
    }
}

pub fn apply_hunk_shifts_to_line_attributions(
    attrs: &[LineAttribution],
    hunks: &[DiffHunk],
) -> Vec<LineAttribution> {
    if hunks.is_empty() {
        return attrs.to_vec();
    }

    let segments = build_preserved_segments(hunks);

    let mut new_attrs: Vec<LineAttribution> = Vec::with_capacity(attrs.len());

    for attr in attrs {
        for &(seg_start, seg_end, seg_offset) in &segments {
            let range_start = attr.start_line.max(seg_start);
            let range_end = attr.end_line.min(seg_end);

            if range_start <= range_end {
                let new_start = (range_start as i64 + seg_offset).max(1) as u32;
                let new_end = (range_end as i64 + seg_offset).max(1) as u32;
                new_attrs.push(LineAttribution {
                    start_line: new_start,
                    end_line: new_end,
                    author_id: attr.author_id.clone(),
                    overrode: attr.overrode.clone(),
                });
            }
        }
    }

    new_attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_range_spec_with_count() {
        assert_eq!(parse_range_spec("10,5"), Some((10, 5)));
        assert_eq!(parse_range_spec("1,0"), Some((1, 0)));
        assert_eq!(parse_range_spec("100,200"), Some((100, 200)));
    }

    #[test]
    fn test_parse_range_spec_without_count() {
        assert_eq!(parse_range_spec("10"), Some((10, 1)));
        assert_eq!(parse_range_spec("1"), Some((1, 1)));
    }

    #[test]
    fn test_parse_range_spec_invalid() {
        assert_eq!(parse_range_spec("abc"), None);
        assert_eq!(parse_range_spec(""), None);
        assert_eq!(parse_range_spec("10,abc"), None);
    }

    #[test]
    fn test_parse_hunk_header_basic() {
        let hunk = parse_hunk_header("@@ -10,5 +12,6 @@ some context").unwrap();
        assert_eq!(hunk.old_start, 10);
        assert_eq!(hunk.old_count, 5);
        assert_eq!(hunk.new_start, 12);
        assert_eq!(hunk.new_count, 6);
    }

    #[test]
    fn test_parse_hunk_header_single_line() {
        let hunk = parse_hunk_header("@@ -5 +5 @@").unwrap();
        assert_eq!(hunk.old_start, 5);
        assert_eq!(hunk.old_count, 1);
        assert_eq!(hunk.new_start, 5);
        assert_eq!(hunk.new_count, 1);
    }

    #[test]
    fn test_parse_hunk_header_insertion_only() {
        let hunk = parse_hunk_header("@@ -3,0 +4,2 @@").unwrap();
        assert_eq!(hunk.old_start, 3);
        assert_eq!(hunk.old_count, 0);
        assert_eq!(hunk.new_start, 4);
        assert_eq!(hunk.new_count, 2);
    }

    #[test]
    fn test_parse_hunk_header_invalid() {
        assert!(parse_hunk_header("not a hunk").is_none());
        assert!(parse_hunk_header("@@ garbage @@").is_none());
    }

    #[test]
    fn test_no_hunks_entries_unchanged() {
        let entries = vec![AttestationEntry::new(
            "abc123".to_string(),
            vec![LineRange::Range(1, 10)],
        )];
        let result = apply_hunk_shifts_to_attestation_entries(&entries, &[]);
        assert_eq!(result, entries);
    }

    #[test]
    fn test_pure_insertion_shifts_lines_after() {
        // Insert 2 lines after line 3
        let hunks = vec![DiffHunk {
            old_start: 3,
            old_count: 0,
            new_start: 4,
            new_count: 2,
        }];

        let entries = vec![
            AttestationEntry::new("a".to_string(), vec![LineRange::Range(1, 3)]),
            AttestationEntry::new("b".to_string(), vec![LineRange::Range(4, 6)]),
        ];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        assert_eq!(result.len(), 2);
        // Lines 1-3 are before/at the insertion point — preserved with no shift
        assert_eq!(result[0].line_ranges, vec![LineRange::Range(1, 3)]);
        // Lines 4-6 are after — shifted by +2
        assert_eq!(result[1].line_ranges, vec![LineRange::Range(6, 8)]);
    }

    #[test]
    fn test_pure_deletion_removes_and_shifts() {
        // Delete lines 3-5 (3 lines)
        let hunks = vec![DiffHunk {
            old_start: 3,
            old_count: 3,
            new_start: 3,
            new_count: 0,
        }];

        let entries = vec![
            AttestationEntry::new("a".to_string(), vec![LineRange::Range(1, 2)]),
            AttestationEntry::new("b".to_string(), vec![LineRange::Range(3, 5)]),
            AttestationEntry::new("c".to_string(), vec![LineRange::Range(6, 8)]),
        ];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        // "a" survives unchanged (lines 1-2 before the hunk)
        // "b" is fully inside the deletion — dropped
        // "c" shifts by -3 (lines 6-8 become 3-5)
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].hash, "a");
        assert_eq!(result[0].line_ranges, vec![LineRange::Range(1, 2)]);
        assert_eq!(result[1].hash, "c");
        assert_eq!(result[1].line_ranges, vec![LineRange::Range(3, 5)]);
    }

    #[test]
    fn test_replacement_drops_replaced_lines() {
        // Replace lines 3-4 with 3 new lines
        let hunks = vec![DiffHunk {
            old_start: 3,
            old_count: 2,
            new_start: 3,
            new_count: 3,
        }];

        let entries = vec![AttestationEntry::new(
            "a".to_string(),
            vec![LineRange::Range(1, 5)],
        )];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        assert_eq!(result.len(), 1);
        // Lines 1-2 preserved at offset 0, lines 3-4 dropped, line 5 shifted by +1
        assert_eq!(
            result[0].line_ranges,
            vec![LineRange::Range(1, 2), LineRange::Single(6)]
        );
    }

    #[test]
    fn test_multiple_hunks_accumulate_offsets() {
        let hunks = vec![
            // Insert 1 line after line 2
            DiffHunk {
                old_start: 2,
                old_count: 0,
                new_start: 3,
                new_count: 1,
            },
            // Delete line 5
            DiffHunk {
                old_start: 5,
                old_count: 1,
                new_start: 6,
                new_count: 0,
            },
        ];

        let entries = vec![
            AttestationEntry::new("a".to_string(), vec![LineRange::Single(1)]),
            AttestationEntry::new("b".to_string(), vec![LineRange::Single(3)]),
            AttestationEntry::new("c".to_string(), vec![LineRange::Single(5)]),
            AttestationEntry::new("d".to_string(), vec![LineRange::Single(6)]),
        ];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        // Line 1: before first hunk, offset 0 → 1
        assert_eq!(result[0].line_ranges, vec![LineRange::Single(1)]);
        // Line 3: between hunks, offset +1 → 4
        assert_eq!(result[1].line_ranges, vec![LineRange::Single(4)]);
        // Line 5: inside second hunk deletion → dropped
        // Line 6: after second hunk, offset +1-1=0 → 6
        assert_eq!(result.len(), 3);
        assert_eq!(result[2].hash, "d");
        assert_eq!(result[2].line_ranges, vec![LineRange::Single(6)]);
    }

    #[test]
    fn test_entry_fully_inside_hunk_removed() {
        let hunks = vec![DiffHunk {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 5,
        }];

        let entries = vec![AttestationEntry::new(
            "doomed".to_string(),
            vec![LineRange::Range(3, 7)],
        )];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        assert!(result.is_empty());
    }

    #[test]
    fn test_file_attestation_returns_none_when_all_emptied() {
        let hunks = vec![DiffHunk {
            old_start: 1,
            old_count: 10,
            new_start: 1,
            new_count: 0,
        }];

        let file = FileAttestation {
            file_path: "foo.rs".to_string(),
            entries: vec![AttestationEntry::new(
                "x".to_string(),
                vec![LineRange::Range(1, 10)],
            )],
        };

        let result = apply_hunk_shifts_to_file_attestation(&file, &hunks);
        assert!(result.is_none());
    }

    #[test]
    fn test_file_attestation_returns_some_when_entries_survive() {
        let hunks = vec![DiffHunk {
            old_start: 5,
            old_count: 2,
            new_start: 5,
            new_count: 0,
        }];

        let file = FileAttestation {
            file_path: "bar.rs".to_string(),
            entries: vec![AttestationEntry::new(
                "x".to_string(),
                vec![LineRange::Range(1, 3)],
            )],
        };

        let result = apply_hunk_shifts_to_file_attestation(&file, &hunks);
        assert!(result.is_some());
        let fa = result.unwrap();
        assert_eq!(fa.file_path, "bar.rs");
        assert_eq!(fa.entries[0].line_ranges, vec![LineRange::Range(1, 3)]);
    }

    #[test]
    fn test_line_attributions_shift() {
        // Delete lines 2-3, insert 1 line in their place
        let hunks = vec![DiffHunk {
            old_start: 2,
            old_count: 2,
            new_start: 2,
            new_count: 1,
        }];

        let attrs = vec![
            LineAttribution::new(1, 1, "human".to_string(), None),
            LineAttribution::new(2, 3, "ai".to_string(), None),
            LineAttribution::new(4, 6, "ai2".to_string(), None),
        ];

        let result = apply_hunk_shifts_to_line_attributions(&attrs, &hunks);
        // Line 1: preserved, offset 0
        assert_eq!(result[0].start_line, 1);
        assert_eq!(result[0].end_line, 1);
        assert_eq!(result[0].author_id, "human");
        // Lines 2-3: inside hunk → dropped
        // Lines 4-6: shifted by -1 → 3-5
        assert_eq!(result.len(), 2);
        assert_eq!(result[1].start_line, 3);
        assert_eq!(result[1].end_line, 5);
        assert_eq!(result[1].author_id, "ai2");
    }

    #[test]
    fn test_line_attributions_no_hunks_unchanged() {
        let attrs = vec![LineAttribution::new(1, 5, "x".to_string(), None)];
        let result = apply_hunk_shifts_to_line_attributions(&attrs, &[]);
        assert_eq!(result, attrs);
    }

    #[test]
    fn test_single_line_range_handling() {
        // Delete line 3
        let hunks = vec![DiffHunk {
            old_start: 3,
            old_count: 1,
            new_start: 3,
            new_count: 0,
        }];

        let entries = vec![
            AttestationEntry::new("a".to_string(), vec![LineRange::Single(2)]),
            AttestationEntry::new("b".to_string(), vec![LineRange::Single(3)]),
            AttestationEntry::new("c".to_string(), vec![LineRange::Single(4)]),
        ];

        let result = apply_hunk_shifts_to_attestation_entries(&entries, &hunks);
        assert_eq!(result.len(), 2);
        assert_eq!(result[0].line_ranges, vec![LineRange::Single(2)]);
        assert_eq!(result[1].line_ranges, vec![LineRange::Single(3)]);
    }
}
