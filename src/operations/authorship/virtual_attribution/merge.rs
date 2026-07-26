use super::types::VirtualAttributions;
use crate::error::GitAiError;
use crate::model::attribution::{ceil_char_boundary, floor_char_boundary};
use crate::model::attribution_tracker::Attribution;
use std::collections::HashMap;

/// Merge two VirtualAttributions, favoring the primary for overlaps
pub fn merge_attributions_favoring_first(
    primary: VirtualAttributions,
    secondary: VirtualAttributions,
    final_state: HashMap<String, String>,
) -> Result<VirtualAttributions, GitAiError> {
    use crate::model::attribution_tracker::AttributionTracker;

    let tracker = AttributionTracker::new();
    let ts = primary.ts;
    let repo = primary.repo.clone();
    let base_commit = primary.base_commit.clone();

    // Merge prompts from both VAs (primary wins on conflict)
    let mut merged_prompts = secondary.prompts.clone();
    for (id, commits) in &primary.prompts {
        merged_prompts.insert(id.clone(), commits.clone());
    }

    // Merge humans from both VAs
    let merged_humans = VirtualAttributions::merge_humans(&primary.humans, &secondary.humans);

    // Merge sessions from both VAs (primary wins on conflict)
    let mut merged_sessions = secondary.sessions.clone();
    for (id, record) in &primary.sessions {
        merged_sessions.insert(id.clone(), record.clone());
    }

    let mut merged = VirtualAttributions {
        repo,
        base_commit,
        attributions: HashMap::new(),
        file_contents: HashMap::new(),
        prompts: merged_prompts,
        ts,
        blame_start_commit: None,
        humans: merged_humans,
        initial_only_prompt_ids: std::collections::HashSet::new(),
        sessions: merged_sessions,
    };

    // Get union of all files
    let mut all_files: std::collections::HashSet<String> =
        primary.attributions.keys().cloned().collect();
    all_files.extend(secondary.attributions.keys().cloned());
    all_files.extend(final_state.keys().cloned());

    for file_path in all_files {
        let final_content = match final_state.get(&file_path) {
            Some(content) => content,
            None => continue, // Skip files not in final state
        };

        // Get attributions from both sources
        let primary_attrs = primary.get_char_attributions(&file_path);
        let secondary_attrs = secondary.get_char_attributions(&file_path);

        // Get source content from both
        let primary_content = primary.get_file_content(&file_path);
        let secondary_content = secondary.get_file_content(&file_path);

        // Transform both to final state
        let transformed_primary =
            if let (Some(attrs), Some(content)) = (primary_attrs, primary_content) {
                transform_attributions_to_final(&tracker, content, attrs, final_content, ts)?
            } else {
                Vec::new()
            };

        let transformed_secondary =
            if let (Some(attrs), Some(content)) = (secondary_attrs, secondary_content) {
                transform_attributions_to_final(&tracker, content, attrs, final_content, ts)?
            } else {
                Vec::new()
            };

        // Merge: primary wins overlaps, secondary fills gaps
        let merged_char_attrs =
            merge_char_attributions(&transformed_primary, &transformed_secondary, final_content);

        // Convert to line attributions
        let merged_line_attrs =
            crate::model::attribution_tracker::attributions_to_line_attributions(
                &merged_char_attrs,
                final_content,
            );

        merged
            .attributions
            .insert(file_path.clone(), (merged_char_attrs, merged_line_attrs));
        merged
            .file_contents
            .insert(file_path, final_content.clone());
    }

    // Save total_additions and total_deletions by summing across sources so squash/rebase preserves totals.
    let mut saved_totals: HashMap<String, (u32, u32)> = HashMap::new();
    for source in [&primary.prompts, &secondary.prompts] {
        for (prompt_id, commits) in source {
            for prompt_record in commits.values() {
                let entry = saved_totals.entry(prompt_id.clone()).or_insert((0, 0));
                entry.0 = entry.0.saturating_add(prompt_record.total_additions);
                entry.1 = entry.1.saturating_add(prompt_record.total_deletions);
            }
        }
    }

    // Calculate and update prompt metrics (will set accepted_lines and overridden_lines)
    VirtualAttributions::calculate_and_update_prompt_metrics(
        &mut merged.prompts,
        &merged.attributions,
        &HashMap::new(), // Empty - will result in total_additions = 0
        &HashMap::new(), // Empty - will result in total_deletions = 0
    );

    // Restore the saved total_additions and total_deletions
    for (prompt_id, commits) in merged.prompts.iter_mut() {
        if let Some(&(additions, deletions)) = saved_totals.get(prompt_id) {
            for prompt_record in commits.values_mut() {
                prompt_record.total_additions = additions;
                prompt_record.total_deletions = deletions;
            }
        }
    }

    Ok(merged)
}

/// Transform attributions from old content to new content
fn transform_attributions_to_final(
    tracker: &crate::model::attribution_tracker::AttributionTracker,
    old_content: &str,
    old_attributions: &[Attribution],
    new_content: &str,
    ts: u128,
) -> Result<Vec<Attribution>, GitAiError> {
    // Use a dummy author for new insertions (we'll discard them anyway)
    let dummy_author = "__DUMMY__";

    let transformed = tracker.update_attributions(
        old_content,
        new_content,
        old_attributions,
        dummy_author,
        ts,
    )?;

    // Filter out dummy attributions (new insertions)
    let filtered: Vec<Attribution> = transformed
        .into_iter()
        .filter(|attr| attr.author_id != dummy_author)
        .collect();

    Ok(filtered)
}

/// Merge character-level attributions, with primary winning overlaps
fn merge_char_attributions(
    primary: &[Attribution],
    secondary: &[Attribution],
    content: &str,
) -> Vec<Attribution> {
    let content_len = content.len();
    if content_len == 0 {
        return primary.to_vec();
    }

    // Create coverage map for primary (byte-based).
    let mut covered = vec![false; content_len];
    #[allow(clippy::needless_range_loop)]
    for attr in primary {
        for i in attr.start..attr.end.min(content_len) {
            covered[i] = true;
        }
    }

    let mut result = Vec::new();

    // Add all primary attributions.
    result.extend(primary.iter().cloned());

    // Add secondary attributions only where primary doesn't cover, on UTF-8 boundaries.
    for attr in secondary {
        let mut range_start: Option<usize> = None;
        let safe_start = floor_char_boundary(content, attr.start);
        let safe_end = ceil_char_boundary(content, attr.end);

        if safe_start >= safe_end {
            continue;
        }

        let slice = &content[safe_start..safe_end];
        for (rel_idx, ch) in slice.char_indices() {
            let start = safe_start + rel_idx;
            let end = start + ch.len_utf8();
            let mut is_covered = false;
            #[allow(clippy::needless_range_loop)]
            for i in start..end.min(content_len) {
                if covered[i] {
                    is_covered = true;
                    break;
                }
            }

            if is_covered {
                if let Some(range_start_idx) = range_start.take()
                    && range_start_idx < start
                {
                    result.push(Attribution::new(
                        range_start_idx,
                        start,
                        attr.author_id.clone(),
                        attr.ts,
                    ));
                }
            } else if range_start.is_none() {
                range_start = Some(start);
            }
        }

        if let Some(range_start_idx) = range_start.take()
            && range_start_idx < safe_end
        {
            result.push(Attribution::new(
                range_start_idx,
                safe_end,
                attr.author_id.clone(),
                attr.ts,
            ));
        }
    }

    // Sort by start position.
    result.sort_by_key(|a| (a.start, a.end));
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secondary_split_byte_range_expands_to_whole_scalar() {
        let secondary = vec![Attribution::new(1, 2, "secondary".to_string(), 1)];

        assert_eq!(
            merge_char_attributions(&[], &secondary, "é"),
            vec![Attribution::new(0, 2, "secondary".to_string(), 1)]
        );
    }

    #[test]
    fn primary_partial_byte_coverage_wins_whole_scalar() {
        let primary = vec![Attribution::new(1, 2, "primary".to_string(), 1)];
        let secondary = vec![Attribution::new(0, 2, "secondary".to_string(), 2)];

        assert_eq!(merge_char_attributions(&primary, &secondary, "é"), primary);
    }

    #[test]
    fn secondary_range_beyond_content_is_ignored() {
        let secondary = vec![Attribution::new(
            usize::MAX,
            usize::MAX,
            "secondary".to_string(),
            1,
        )];

        assert!(merge_char_attributions(&[], &secondary, "é").is_empty());
    }
}
