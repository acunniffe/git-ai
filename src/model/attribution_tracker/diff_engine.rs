//! Diff computation engine.
//!
//! Provides `DiffComputation` (the accumulator filled by the tracker's diff
//! phases), the hunk-classification predicates, byte-range helpers, and the
//! token-aligned diff builder that drives fine-grained attribution.

use crate::model::imara_diff_utils::{ByteDiff, ByteDiffOp, DiffOp, capture_diff_slices};

use super::tokenizer::{LineMetadata, tokenize_non_whitespace};

pub(super) const MOVE_DETECTION_MIN_FILE_BYTES: usize = 64 * 1024;
pub(super) const MOVE_DETECTION_MAX_OPS: usize = 256;
pub(super) const TOKEN_DIFF_FAST_PATH_MIN_BYTES: usize = 32 * 1024;
pub(super) const TOKEN_DIFF_FAST_PATH_MIN_LINES: usize = 256;
pub(super) const TOKEN_DIFF_FAST_PATH_HUGE_BYTES: usize = 256 * 1024;
pub(super) const TOKEN_DIFF_FAST_PATH_MAX_OPS: usize = 8;

#[derive(Default)]
pub(super) struct DiffComputation {
    pub(super) diffs: Vec<ByteDiff>,
    pub(super) substantive_new_ranges: Vec<(usize, usize)>,
}

pub(super) fn line_span_for_op(op: &DiffOp, for_old: bool) -> (usize, usize) {
    match (op, for_old) {
        (DiffOp::Equal { old_index, len, .. }, true) => (*old_index, *old_index + *len),
        (DiffOp::Equal { new_index, len, .. }, false) => (*new_index, *new_index + *len),
        (
            DiffOp::Delete {
                old_index, old_len, ..
            },
            true,
        ) => (*old_index, *old_index + *old_len),
        (DiffOp::Delete { new_index, .. }, false) => (*new_index, *new_index),
        (DiffOp::Insert { old_index, .. }, true) => (*old_index, *old_index),
        (
            DiffOp::Insert {
                new_index, new_len, ..
            },
            false,
        ) => (*new_index, *new_index + *new_len),
        (
            DiffOp::Replace {
                old_index, old_len, ..
            },
            true,
        ) => (*old_index, *old_index + *old_len),
        (
            DiffOp::Replace {
                new_index, new_len, ..
            },
            false,
        ) => (*new_index, *new_index + *new_len),
    }
}

pub(super) fn hunk_line_bounds(ops: &[DiffOp], for_old: bool) -> (usize, usize) {
    let mut start = usize::MAX;
    let mut end = 0usize;

    for op in ops {
        let (s, e) = line_span_for_op(op, for_old);
        start = start.min(s);
        end = end.max(e);
    }

    if start == usize::MAX {
        (0, 0)
    } else {
        (start, end)
    }
}

pub(super) fn should_use_line_aligned_hunk_diff(
    ops: &[DiffOp],
    changed_old_lines: usize,
    changed_new_lines: usize,
    changed_old_bytes: usize,
    changed_new_bytes: usize,
) -> bool {
    if ops.is_empty() || ops.len() > TOKEN_DIFF_FAST_PATH_MAX_OPS {
        return false;
    }

    let changed_lines = changed_old_lines.max(changed_new_lines);
    let changed_bytes = changed_old_bytes.max(changed_new_bytes);

    changed_bytes >= TOKEN_DIFF_FAST_PATH_HUGE_BYTES
        || (changed_bytes >= TOKEN_DIFF_FAST_PATH_MIN_BYTES
            && changed_lines >= TOKEN_DIFF_FAST_PATH_MIN_LINES)
}

pub(super) fn line_range_to_byte_range(
    lines: &[LineMetadata],
    start_idx: usize,
    end_idx: usize,
    content_len: usize,
) -> (usize, usize) {
    if start_idx >= end_idx {
        let pos = lines
            .get(start_idx)
            .map(|line| line.start)
            .unwrap_or(content_len);
        return (pos, pos);
    }

    let start = lines
        .get(start_idx)
        .map(|line| line.start)
        .unwrap_or(content_len);
    let end_line = end_idx.saturating_sub(1);
    let end = lines
        .get(end_line)
        .map(|line| line.end)
        .unwrap_or(content_len);

    (start, end)
}

pub(super) fn append_range_diffs(
    diffs: &mut Vec<ByteDiff>,
    old_content: &str,
    new_content: &str,
    old_range: (usize, usize),
    new_range: (usize, usize),
    force_split: bool,
) {
    let (old_start, old_end) = old_range;
    let (new_start, new_end) = new_range;

    if old_start >= old_end && new_start >= new_end {
        return;
    }

    let old_slice = &old_content[old_start..old_end];
    let new_slice = &new_content[new_start..new_end];

    if !force_split && !old_slice.is_empty() && !new_slice.is_empty() && old_slice == new_slice {
        diffs.push(ByteDiff::new(ByteDiffOp::Equal, new_slice.as_bytes()));
        return;
    }

    if !old_slice.is_empty() {
        diffs.push(ByteDiff::new(ByteDiffOp::Delete, old_slice.as_bytes()));
    }
    if !new_slice.is_empty() {
        diffs.push(ByteDiff::new(ByteDiffOp::Insert, new_slice.as_bytes()));
    }
}

pub(super) fn build_token_aligned_diffs(
    old_content: &str,
    new_content: &str,
    old_range: (usize, usize),
    new_range: (usize, usize),
) -> (Vec<ByteDiff>, Vec<(usize, usize)>) {
    let (old_start, old_end) = old_range;
    let (new_start, new_end) = new_range;

    let mut diffs = Vec::new();
    let mut substantive_ranges = Vec::new();

    let old_tokens = tokenize_non_whitespace(old_content, old_range);
    let new_tokens = tokenize_non_whitespace(new_content, new_range);

    if old_tokens.is_empty() && new_tokens.is_empty() {
        append_range_diffs(
            &mut diffs,
            old_content,
            new_content,
            (old_start, old_end),
            (new_start, new_end),
            false,
        );
        return (diffs, substantive_ranges);
    }

    let token_ops = capture_diff_slices(&old_tokens, &new_tokens);
    let mut old_cursor = old_start;
    let mut new_cursor = new_start;
    let mut last_was_change = false;

    for op in token_ops {
        match op {
            DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for i in 0..len {
                    let old_token = &old_tokens[old_index + i];
                    let new_token = &new_tokens[new_index + i];

                    append_range_diffs(
                        &mut diffs,
                        old_content,
                        new_content,
                        (old_cursor, old_token.start),
                        (new_cursor, new_token.start),
                        last_was_change,
                    );

                    diffs.push(ByteDiff::new(
                        ByteDiffOp::Equal,
                        &new_content.as_bytes()[new_token.start..new_token.end],
                    ));

                    old_cursor = old_token.end;
                    new_cursor = new_token.end;
                    last_was_change = false;
                }
            }
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                if old_len == 0 {
                    continue;
                }

                let start = old_tokens[old_index].start;
                let end = old_tokens[old_index + old_len - 1].end;

                append_range_diffs(
                    &mut diffs,
                    old_content,
                    new_content,
                    (old_cursor, start),
                    (new_cursor, new_cursor),
                    last_was_change,
                );

                diffs.push(ByteDiff::new(
                    ByteDiffOp::Delete,
                    &old_content.as_bytes()[start..end],
                ));

                old_cursor = end;
                last_was_change = true;
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                if new_len == 0 {
                    continue;
                }

                let start = new_tokens[new_index].start;
                let end = new_tokens[new_index + new_len - 1].end;

                append_range_diffs(
                    &mut diffs,
                    old_content,
                    new_content,
                    (old_cursor, old_cursor),
                    (new_cursor, start),
                    last_was_change,
                );

                diffs.push(ByteDiff::new(
                    ByteDiffOp::Insert,
                    &new_content.as_bytes()[start..end],
                ));

                substantive_ranges.push((start, end));
                new_cursor = end;
                last_was_change = true;
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                let old_start_pos = old_tokens
                    .get(old_index)
                    .map(|t| t.start)
                    .unwrap_or(old_cursor);
                let new_start_pos = new_tokens
                    .get(new_index)
                    .map(|t| t.start)
                    .unwrap_or(new_cursor);

                append_range_diffs(
                    &mut diffs,
                    old_content,
                    new_content,
                    (old_cursor, old_start_pos),
                    (new_cursor, new_start_pos),
                    last_was_change,
                );

                if old_len > 0 {
                    let old_end_pos = old_tokens[old_index + old_len - 1].end;
                    diffs.push(ByteDiff::new(
                        ByteDiffOp::Delete,
                        &old_content.as_bytes()[old_start_pos..old_end_pos],
                    ));
                    old_cursor = old_end_pos;
                } else {
                    old_cursor = old_start_pos;
                }

                if new_len > 0 {
                    let new_end_pos = new_tokens[new_index + new_len - 1].end;
                    diffs.push(ByteDiff::new(
                        ByteDiffOp::Insert,
                        &new_content.as_bytes()[new_start_pos..new_end_pos],
                    ));
                    substantive_ranges.push((new_start_pos, new_end_pos));
                    new_cursor = new_end_pos;
                } else {
                    new_cursor = new_start_pos;
                }
                last_was_change = true;
            }
        }
    }

    append_range_diffs(
        &mut diffs,
        old_content,
        new_content,
        (old_cursor, old_end),
        (new_cursor, new_end),
        last_was_change,
    );

    (diffs, substantive_ranges)
}

pub(super) fn merge_ranges(mut ranges: Vec<(usize, usize)>) -> Vec<(usize, usize)> {
    if ranges.is_empty() {
        return ranges;
    }

    ranges.sort_by_key(|r| (r.0, r.1));
    let mut merged: Vec<(usize, usize)> = Vec::new();

    for (start, end) in ranges {
        if start >= end {
            continue;
        }

        if let Some(last) = merged.last_mut() {
            if start <= last.1 {
                last.1 = last.1.max(end);
            } else {
                merged.push((start, end));
            }
        } else {
            merged.push((start, end));
        }
    }

    merged
}

pub(super) fn ranges_intersect(ranges: &[(usize, usize)], target: (usize, usize)) -> bool {
    let (start, end) = target;
    if start >= end {
        return false;
    }

    for &(r_start, r_end) in ranges {
        if r_end <= start {
            continue;
        }
        if r_start >= end {
            return false;
        }
        return true;
    }

    false
}

pub(super) fn data_is_whitespace(data: &[u8]) -> bool {
    if data.is_empty() {
        return false;
    }

    std::str::from_utf8(data)
        .map(|s| s.chars().all(|c| c.is_whitespace()))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::merge_ranges;

    #[test]
    fn merge_ranges_sorts_merges_adjacent_and_discards_empty_ranges() {
        assert_eq!(
            merge_ranges(vec![
                (8, 10),
                (2, 5),
                (5, 8),
                (3, 4),
                (9, 9),
                (12, 11),
                (13, 15)
            ]),
            vec![(2, 10), (13, 15)]
        );
    }
}
