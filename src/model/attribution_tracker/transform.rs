//! Attribution transformation — phase 4.
//!
//! `transform_attributions` walks the `ByteDiff` sequence and shifts
//! existing attribution ranges into their new positions, handling equal
//! (copy), delete (move-detection or marker), and insert (new author or
//! inherited) operations.
//!
//! `find_attribution_for_insertion` is a cursor-accelerated lookup used
//! inside the insert arm of the transform loop.

use std::collections::HashMap;

use super::diff_engine::{data_is_whitespace, merge_ranges, ranges_intersect};
use super::tracker::AttributionTracker;
use super::types::{Insertion, MoveMapping};
use crate::model::attribution::Attribution;
use crate::model::imara_diff_utils::{ByteDiff, ByteDiffOp};

impl AttributionTracker {
    /// Transform attributions through the diff
    #[allow(clippy::too_many_arguments)]
    pub(super) fn transform_attributions(
        &self,
        diffs: &[ByteDiff],
        old_attributions: &[Attribution],
        current_author: &str,
        insertions: &[Insertion],
        move_mappings: &[MoveMapping],
        ts: u128,
        substantive_new_ranges: &[(usize, usize)],
        is_ai_checkpoint: bool,
    ) -> Vec<Attribution> {
        let mut new_attributions = Vec::new();

        // Build lookup maps for moves
        let mut deletion_to_move: HashMap<usize, Vec<&MoveMapping>> = HashMap::new();
        let mut insertion_move_ranges: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();

        for mapping in move_mappings {
            let entry = deletion_to_move.entry(mapping.deletion_idx).or_default();
            if !entry.iter().any(|existing| {
                existing.source_range == mapping.source_range
                    && existing.target_range == mapping.target_range
            }) {
                entry.push(mapping);
            }
            insertion_move_ranges
                .entry(mapping.insertion_idx)
                .or_default()
                .push(mapping.target_range);
        }

        for mappings in deletion_to_move.values_mut() {
            mappings.sort_by_key(|m| m.source_range.0);
        }

        let mut old_pos = 0;
        let mut new_pos = 0;
        let mut deletion_idx = 0;
        let mut insertion_idx = 0;
        let mut prev_whitespace_delete = false;
        let mut old_attr_cursor = 0usize;
        let mut insertion_attr_cursor = 0usize;

        for diff in diffs {
            let op = diff.op();
            let len = diff.data().len();

            match op {
                ByteDiffOp::Equal => {
                    // Unchanged text: transform attributions directly
                    let old_range = (old_pos, old_pos + len);
                    let new_range = (new_pos, new_pos + len);

                    while old_attr_cursor < old_attributions.len()
                        && old_attributions[old_attr_cursor].end <= old_range.0
                    {
                        old_attr_cursor += 1;
                    }
                    let mut attr_idx = old_attr_cursor;
                    while attr_idx < old_attributions.len() {
                        let attr = &old_attributions[attr_idx];
                        if attr.start >= old_range.1 {
                            break;
                        }
                        if let Some((overlap_start, overlap_end)) =
                            attr.intersection(old_range.0, old_range.1)
                        {
                            // Transform to new position
                            let offset_in_range = overlap_start - old_range.0;
                            let overlap_len = overlap_end - overlap_start;

                            new_attributions.push(Attribution::new(
                                new_range.0 + offset_in_range,
                                new_range.0 + offset_in_range + overlap_len,
                                attr.author_id.clone(),
                                attr.ts,
                            ));
                        }
                        attr_idx += 1;
                    }

                    old_pos += len;
                    new_pos += len;
                    prev_whitespace_delete = false;
                }
                ByteDiffOp::Delete => {
                    let deletion_range = (old_pos, old_pos + len);

                    // Check if this deletion is part of a move
                    if let Some(mappings) = deletion_to_move.get(&deletion_idx) {
                        for mapping in mappings {
                            let insertion = &insertions[mapping.insertion_idx];
                            let source_start = deletion_range.0 + mapping.source_range.0;
                            let source_end = deletion_range.0 + mapping.source_range.1;

                            if source_start < source_end {
                                let target_start = insertion.start + mapping.target_range.0;

                                while old_attr_cursor < old_attributions.len()
                                    && old_attributions[old_attr_cursor].end <= source_start
                                {
                                    old_attr_cursor += 1;
                                }
                                let mut attr_idx = old_attr_cursor;
                                while attr_idx < old_attributions.len() {
                                    let attr = &old_attributions[attr_idx];
                                    if attr.start >= source_end {
                                        break;
                                    }
                                    if let Some((overlap_start, overlap_end)) =
                                        attr.intersection(source_start, source_end)
                                    {
                                        let offset_in_source = overlap_start - source_start;
                                        let new_start = target_start + offset_in_source;
                                        let new_end = new_start + (overlap_end - overlap_start);

                                        if new_start < new_end {
                                            new_attributions.push(Attribution::new(
                                                new_start,
                                                new_end,
                                                attr.author_id.clone(),
                                                attr.ts,
                                            ));
                                        }
                                    }
                                    attr_idx += 1;
                                }
                            }
                        }
                    } else if is_ai_checkpoint || !data_is_whitespace(diff.data()) {
                        // For non-move deletions of substantive content, create a zero-length
                        // marker attribution at the deletion point. For AI checkpoints, apply
                        // this to whitespace deletions as well so formatting-only rewrites are
                        // attributed to AI.
                        new_attributions.push(Attribution::new(
                            new_pos,
                            new_pos, // Zero-length marker
                            current_author.to_string(),
                            ts,
                        ));
                    }

                    old_pos += len;
                    deletion_idx += 1;
                    prev_whitespace_delete = data_is_whitespace(diff.data());
                }
                ByteDiffOp::Insert => {
                    // Check if this insertion is from a detected move
                    if let Some(ranges) = insertion_move_ranges.remove(&insertion_idx) {
                        let merged = merge_ranges(ranges);

                        let mut cursor = 0usize;
                        for (start, end) in merged {
                            let clamped_start = start.min(len);
                            let clamped_end = end.min(len);

                            if cursor < clamped_start {
                                new_attributions.push(Attribution::new(
                                    new_pos + cursor,
                                    new_pos + clamped_start,
                                    current_author.to_string(),
                                    ts,
                                ));
                            }

                            cursor = cursor.max(clamped_end);
                        }

                        if cursor < len {
                            new_attributions.push(Attribution::new(
                                new_pos + cursor,
                                new_pos + len,
                                current_author.to_string(),
                                ts,
                            ));
                        }

                        new_pos += len;
                        insertion_idx += 1;
                        prev_whitespace_delete = false;
                        continue;
                    }

                    // Add attribution for this insertion
                    if is_ai_checkpoint {
                        new_attributions.push(Attribution::new(
                            new_pos,
                            new_pos + len,
                            current_author.to_string(),
                            ts,
                        ));

                        new_pos += len;
                        insertion_idx += 1;
                        prev_whitespace_delete = false;
                        continue;
                    }

                    let insertion_range = (new_pos, new_pos + len);
                    let is_substantive_insert =
                        ranges_intersect(substantive_new_ranges, insertion_range);
                    let is_whitespace_only = data_is_whitespace(diff.data());
                    let contains_newline = diff.data().contains(&b'\n');
                    let is_formatting_pair = prev_whitespace_delete && is_whitespace_only;
                    #[allow(clippy::if_same_then_else)]
                    let (author_id, attribution_ts) = if contains_newline {
                        (current_author.to_string(), ts)
                    } else if is_substantive_insert {
                        (current_author.to_string(), ts)
                    } else if is_formatting_pair {
                        if let Some(attr) = find_attribution_for_insertion(
                            old_attributions,
                            old_pos,
                            &mut insertion_attr_cursor,
                        ) {
                            (attr.author_id.clone(), attr.ts)
                        } else if let Some(attr) = new_attributions.last() {
                            (attr.author_id.clone(), attr.ts)
                        } else {
                            (current_author.to_string(), ts)
                        }
                    } else if let Some(attr) = new_attributions.last() {
                        (attr.author_id.clone(), attr.ts)
                    } else if let Some(attr) = find_attribution_for_insertion(
                        old_attributions,
                        old_pos,
                        &mut insertion_attr_cursor,
                    ) {
                        (attr.author_id.clone(), attr.ts)
                    } else {
                        (current_author.to_string(), ts)
                    };

                    new_attributions.push(Attribution::new(
                        new_pos,
                        new_pos + len,
                        author_id,
                        attribution_ts,
                    ));

                    new_pos += len;
                    insertion_idx += 1;
                    prev_whitespace_delete = false;
                }
            }
        }

        new_attributions
    }
}

fn find_attribution_for_insertion<'a>(
    old_attributions: &'a [Attribution],
    position: usize,
    cursor_hint: &mut usize,
) -> Option<&'a Attribution> {
    if old_attributions.is_empty() {
        return None;
    }

    while *cursor_hint < old_attributions.len() && old_attributions[*cursor_hint].end <= position {
        *cursor_hint += 1;
    }

    let mut best_overlap: Option<&Attribution> = None;
    let mut idx = *cursor_hint;
    while idx < old_attributions.len() {
        let attr = &old_attributions[idx];
        if attr.start > position {
            break;
        }
        let better_than_current = match best_overlap {
            None => true,
            Some(best) => {
                attr.ts > best.ts
                    || (attr.ts == best.ts && (attr.end - attr.start) > (best.end - best.start))
            }
        };
        if attr.overlaps(position, position.saturating_add(1)) && better_than_current {
            best_overlap = Some(attr);
        }
        idx += 1;
    }

    if best_overlap.is_some() {
        return best_overlap;
    }

    let before = if *cursor_hint > 0 {
        Some(&old_attributions[*cursor_hint - 1])
    } else {
        None
    };
    let after = old_attributions
        .iter()
        .skip(*cursor_hint)
        .find(|a| a.start >= position);

    before.or(after)
}
