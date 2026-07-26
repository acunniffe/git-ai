//! Line-granularity attribution conversion.
//!
//! Converts between character-offset `Attribution` vectors and 1-indexed
//! `LineAttribution` ranges.  All logic is pure algebra over the content
//! string and attribution slices; no I/O or git calls occur here.

use crate::model::attribution::{
    Attribution, LineAttribution, ceil_char_boundary, floor_char_boundary,
};
use crate::model::working_log::CheckpointKind;

/// Helper struct to track line boundaries in content
struct LineBoundaries {
    /// Maps line number (1-indexed) to (start_byte, end_byte) exclusive end
    line_ranges: Vec<(usize, usize)>,
}

impl LineBoundaries {
    fn new(content: &str) -> Self {
        let mut line_ranges = Vec::new();
        let mut start = 0;

        for (idx, _) in content.match_indices('\n') {
            // Line from start to idx (inclusive of newline)
            line_ranges.push((start, idx + 1));
            start = idx + 1;
        }

        // Handle last line if it doesn't end with newline
        if start < content.len() {
            line_ranges.push((start, content.len()));
        } else if start == content.len() && content.is_empty() {
            // Empty file - no lines
        } else if start == content.len() && !content.is_empty() {
            // File ends with newline, last line is already added
        }

        LineBoundaries { line_ranges }
    }

    fn line_count(&self) -> u32 {
        self.line_ranges.len() as u32
    }

    fn get_line_range(&self, line_num: u32) -> Option<(usize, usize)> {
        if line_num < 1 || line_num as usize > self.line_ranges.len() {
            None
        } else {
            Some(self.line_ranges[line_num as usize - 1])
        }
    }
}

/// Convert line-based attributions to character-based attributions.
///
/// # Arguments
/// * `line_attributions` - Line-based attributions to convert
/// * `content` - The file content to map line numbers to character positions
///
/// # Returns
/// A vector of character-based attributions covering the same ranges
pub fn line_attributions_to_attributions(
    line_attributions: &Vec<LineAttribution>,
    content: &str,
    ts: u128,
) -> Vec<Attribution> {
    if line_attributions.is_empty() || content.is_empty() {
        return Vec::new();
    }

    let boundaries = LineBoundaries::new(content);
    let mut result = Vec::new();

    for line_attr in line_attributions {
        // Get character ranges for start and end lines
        let start_range = boundaries.get_line_range(line_attr.start_line);
        let end_range = boundaries.get_line_range(line_attr.end_line);

        if let (Some((start_char, _)), Some((_, end_char))) = (start_range, end_range) {
            result.push(Attribution::new(
                start_char,
                end_char,
                line_attr.author_id.clone(),
                ts,
            ));
        }
    }

    result
}

/// Convert character-based attributions to line-based attributions.
/// For each line, selects the "dominant" author based on who contributed
/// the most non-whitespace characters to that line.
/// Finally, strip away all human-authored lines that aren't overrides.
///
/// # Arguments
/// * `attributions` - Character-based attributions
/// * `content` - The file content being attributed
///
/// # Returns
/// A vector of line attributions with consecutive lines by the same author merged
pub fn attributions_to_line_attributions(
    attributions: &[Attribution],
    content: &str,
) -> Vec<LineAttribution> {
    attributions_to_line_attributions_for_checkpoint(attributions, content, false)
}

pub fn attributions_to_line_attributions_for_checkpoint(
    attributions: &[Attribution],
    content: &str,
    is_ai_checkpoint: bool,
) -> Vec<LineAttribution> {
    if content.is_empty() || attributions.is_empty() {
        return Vec::new();
    }

    let boundaries = LineBoundaries::new(content);
    let line_count = boundaries.line_count();

    if line_count == 0 {
        return Vec::new();
    }

    let mut sorted_indices: Vec<usize> = (0..attributions.len()).collect();
    sorted_indices.sort_by_key(|&idx| (attributions[idx].start, attributions[idx].end, idx));

    let mut next_idx = 0usize;
    let mut active_indices: Vec<usize> = Vec::new();

    // For each line, determine the dominant author using a sweep over overlapping ranges.
    let mut line_authors: Vec<Option<(String, Option<String>)>> =
        Vec::with_capacity(line_count as usize);

    for line_num in 1..=line_count {
        let Some((line_start, line_end)) = boundaries.get_line_range(line_num) else {
            line_authors.push(Some((CheckpointKind::Human.to_str(), None)));
            continue;
        };

        while next_idx < sorted_indices.len()
            && attributions[sorted_indices[next_idx]].start < line_end
        {
            active_indices.push(sorted_indices[next_idx]);
            next_idx += 1;
        }

        active_indices.retain(|&attr_idx| {
            let attr = &attributions[attr_idx];
            attr.start < line_end && attr.end > line_start
        });

        let line_content = &content[line_start..line_end];
        let is_line_empty =
            line_content.is_empty() || line_content.chars().all(|c| c.is_whitespace());
        let (author, overrode) = find_dominant_author_for_line_candidates(
            line_start,
            line_end,
            is_line_empty,
            &active_indices,
            attributions,
            content,
            is_ai_checkpoint,
        );
        line_authors.push(Some((author, overrode)));
    }

    // Merge consecutive lines with the same author
    let mut merged_line_authors = merge_consecutive_line_attributions(line_authors);

    // Strip away all human lines (only AI lines need to be retained)
    merged_line_authors.retain(|line_attr| {
        line_attr.author_id != CheckpointKind::Human.to_str() || line_attr.overrode.is_some()
    });
    merged_line_authors
}

/// Find the dominant author for a specific line from overlapping attribution candidates.
fn find_dominant_author_for_line_candidates(
    line_start: usize,
    line_end: usize,
    is_line_empty: bool,
    candidate_indices: &[usize],
    attributions: &[Attribution],
    full_content: &str,
    is_ai_checkpoint: bool,
) -> (String, Option<String>) {
    let mut candidate_attrs: Vec<&Attribution> = Vec::new();
    for &attr_idx in candidate_indices {
        let attribution = &attributions[attr_idx];
        if !attribution.overlaps(line_start, line_end) {
            continue;
        }

        // Get the substring of the content on this line that is covered by the attribution
        let slice_start = std::cmp::max(line_start, attribution.start);
        let slice_end = std::cmp::min(line_end, attribution.end);
        let mut has_non_whitespace = false;
        if slice_start < slice_end {
            let safe_start = if full_content.is_char_boundary(slice_start) {
                slice_start
            } else {
                floor_char_boundary(full_content, slice_start).max(line_start)
            };
            let safe_end = if full_content.is_char_boundary(slice_end) {
                slice_end
            } else {
                ceil_char_boundary(full_content, slice_end).min(line_end)
            };

            if safe_start < safe_end {
                let content_slice = &full_content[safe_start..safe_end];
                has_non_whitespace = content_slice.chars().any(|c| !c.is_whitespace());
            }
        }
        // Zero-length attributions are deletion markers - they indicate the author
        // deleted content at this position, so they should influence line attribution
        let is_deletion_marker = attribution.start == attribution.end;
        // h_<hash> IDs are known-human attestations; treat them like "human" for
        // whitespace-inclusion purposes so their newline ranges don't bleed into
        // adjacent lines during an AI checkpoint.
        let is_ai_author = attribution.author_id != CheckpointKind::Human.to_str()
            && !attribution.author_id.starts_with("h_");
        let include_ai_whitespace = is_ai_checkpoint && is_ai_author;
        if has_non_whitespace || is_line_empty || is_deletion_marker || include_ai_whitespace {
            candidate_attrs.push(attribution);
        } else {
            // If the attribution is only whitespace, discard it
            continue;
        }
    }

    if candidate_attrs.is_empty() {
        return (CheckpointKind::Human.to_str(), None);
    }

    // Choose the author with the latest timestamp (keep first match on ties).
    let mut latest_author = candidate_attrs[0];
    for attr in candidate_attrs.iter().skip(1) {
        if attr.ts > latest_author.ts {
            latest_author = attr;
        }
    }

    let mut last_ai_edit: Option<&Attribution> = None;
    let mut last_human_edit: Option<&Attribution> = None;
    for attr in &candidate_attrs {
        // Both legacy "human" and KnownHuman h_<hash> IDs are human edits.
        if attr.author_id == CheckpointKind::Human.to_str() || attr.author_id.starts_with("h_") {
            last_human_edit = Some(attr);
        } else {
            last_ai_edit = Some(attr);
        }
    }
    let overrode = match (last_ai_edit, last_human_edit) {
        (Some(ai), Some(h)) => {
            if h.ts > ai.ts {
                Some(ai.author_id.clone())
            } else {
                None
            }
        }
        _ => None,
    };
    (latest_author.author_id.clone(), overrode)
}

/// Merge consecutive lines with the same author into LineAttribution ranges
fn merge_consecutive_line_attributions(
    line_authorship: Vec<Option<(String, Option<String>)>>,
) -> Vec<LineAttribution> {
    let mut result = Vec::new();
    let line_count = line_authorship.len();

    let mut current_authorship: Option<(String, Option<String>)> = None;
    let mut current_start: u32 = 0;

    for (idx, authorship) in line_authorship.into_iter().enumerate() {
        let line_num = (idx + 1) as u32;

        match (&current_authorship, authorship) {
            (None, None) => {
                // No attribution for this line, continue
            }
            (None, Some(new_author)) => {
                // Start a new line attribution
                current_authorship = Some(new_author);
                current_start = line_num;
            }
            (Some(_), None) => {
                // End current attribution
                if let Some(authorship) = current_authorship.take() {
                    result.push(LineAttribution::new(
                        current_start,
                        line_num - 1,
                        authorship.0,
                        authorship.1,
                    ));
                }
            }
            (Some(curr), Some(new_authorship)) => {
                if curr == &new_authorship {
                    // Continue current attribution
                } else {
                    // End current, start new
                    result.push(LineAttribution::new(
                        current_start,
                        line_num - 1,
                        curr.0.clone(),
                        curr.1.clone(),
                    ));
                    current_authorship = Some(new_authorship);
                    current_start = line_num;
                }
            }
        }
    }

    // Close final attribution if any
    if let Some(authorship) = current_authorship {
        result.push(LineAttribution::new(
            current_start,
            line_count as u32,
            authorship.0,
            authorship.1,
        ));
    }

    result
}
