use crate::model::hunk_shift::DiffHunk;
use crate::model::imara_diff_utils::{
    content_eq_ignoring_line_endings, normalize_line_endings, split_lines_with_terminators,
};

pub(super) fn diff_hunks_between_contents(old_content: &str, new_content: &str) -> Vec<DiffHunk> {
    let normalized_old = normalize_line_endings(old_content);
    let normalized_new = normalize_line_endings(new_content);
    let old_lines = split_lines_with_terminators(&normalized_old);
    let new_lines = split_lines_with_terminators(&normalized_new);
    crate::model::imara_diff_utils::capture_diff_slices(&old_lines, &new_lines)
        .into_iter()
        .filter_map(|op| match op {
            crate::model::imara_diff_utils::DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => Some(DiffHunk {
                old_start: old_index as u32,
                old_count: 0,
                new_start: new_index as u32 + 1,
                new_count: new_len as u32,
            }),
            crate::model::imara_diff_utils::DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => Some(DiffHunk {
                old_start: old_index as u32 + 1,
                old_count: old_len as u32,
                new_start: new_index as u32 + 1,
                new_count: 0,
            }),
            crate::model::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => Some(DiffHunk {
                old_start: old_index as u32 + 1,
                old_count: old_len as u32,
                new_start: new_index as u32 + 1,
                new_count: new_len as u32,
            }),
            crate::model::imara_diff_utils::DiffOp::Equal { .. } => None,
        })
        .collect()
}

fn line_sequence_contains(needle: &str, haystack: &str) -> bool {
    let normalized_needle = normalize_line_endings(needle);
    let normalized_haystack = normalize_line_endings(haystack);
    let needle_lines = split_lines_with_terminators(&normalized_needle);
    if needle_lines.is_empty() {
        return true;
    }

    let mut next_needle = 0;
    for haystack_line in split_lines_with_terminators(&normalized_haystack) {
        if haystack_line == needle_lines[next_needle] {
            next_needle += 1;
            if next_needle == needle_lines.len() {
                return true;
            }
        }
    }
    false
}

/// Pure carryover reconciliation given already-fetched contents (no git ops).
/// `parent_content` is the file at the parent commit ("" if absent / initial).
pub(crate) fn merged_carryover_content_pure(
    parent_content: &str,
    committed_content: &str,
    observed_content: &str,
) -> String {
    if content_eq_ignoring_line_endings(committed_content, observed_content) {
        return committed_content.to_string();
    }
    if line_sequence_contains(committed_content, observed_content) {
        return observed_content.to_string();
    }
    if line_sequence_contains(observed_content, committed_content) {
        return committed_content.to_string();
    }
    if content_eq_ignoring_line_endings(committed_content, parent_content) {
        return observed_content.to_string();
    }
    if content_eq_ignoring_line_endings(observed_content, parent_content) {
        return committed_content.to_string();
    }
    carryover_merge_content(parent_content, committed_content, observed_content)
}

/// In-memory 3-way line merge replacing a per-file `git merge-file --theirs -p
/// <committed> <parent> <observed>` spawn (base = `parent`, "ours" =
/// `committed`, favored "theirs" = `observed`). Implements the standard diff3
/// chunk algorithm: align both sides to the base, walk base regions, take the
/// changed side for one-sided changes, and resolve two-sided (conflicting)
/// changes to the observed side. The result feeds an in-memory diff for line
/// bucketing (not stored as an authoritative blob), so byte-exact parity with
/// git's conflict formatting is not required — only a faithful clean-merge
/// reconstruction. Keeps the carryover snapshot build free of per-file spawns.
pub(super) fn carryover_merge_content(parent: &str, committed: &str, observed: &str) -> String {
    use crate::model::imara_diff_utils::{DiffOp, capture_diff_slices};

    if content_eq_ignoring_line_endings(committed, observed) {
        return committed.to_string();
    }
    if content_eq_ignoring_line_endings(parent, committed) {
        return observed.to_string();
    }
    if content_eq_ignoring_line_endings(parent, observed) {
        return committed.to_string();
    }

    let base_lines = split_lines_with_terminators(parent);
    let committed_lines = split_lines_with_terminators(committed);
    let observed_lines = split_lines_with_terminators(observed);
    let normalized_parent = normalize_line_endings(parent);
    let normalized_committed = normalize_line_endings(committed);
    let normalized_observed = normalize_line_endings(observed);
    let base_cmp_lines = split_lines_with_terminators(&normalized_parent);
    let committed_cmp_lines = split_lines_with_terminators(&normalized_committed);
    let observed_cmp_lines = split_lines_with_terminators(&normalized_observed);

    // For each side, map every base line index to its aligned index on that
    // side (None if the base line was changed/deleted on that side). Also record
    // each side's content so we can emit it for changed chunks.
    fn align_to_base(base: &[&str], side: &[&str]) -> Vec<Option<usize>> {
        let base_len = base.len();
        let mut map = vec![None; base_len];
        for op in capture_diff_slices(base, side) {
            if let DiffOp::Equal {
                old_index,
                new_index,
                len,
            } = op
            {
                for k in 0..len {
                    map[old_index + k] = Some(new_index + k);
                }
            }
        }
        map
    }

    let committed_map = align_to_base(&base_cmp_lines, &committed_cmp_lines);
    let observed_map = align_to_base(&base_cmp_lines, &observed_cmp_lines);

    // A base line is "stable" when both sides keep it aligned (unchanged on
    // both). We walk base lines; runs of stable lines are emitted verbatim,
    // and the gaps between them are chunks where at least one side changed.
    // Within each chunk we also consume the corresponding side lines (between
    // the surrounding stable anchors) so inserts/edits are captured.
    let mut result: Vec<String> = Vec::new();
    let mut base_i = 0usize;
    let mut committed_i = 0usize; // next unconsumed committed line
    let mut observed_i = 0usize; // next unconsumed observed line

    // Helper: is base line `i` stable (aligned on both sides)?
    let is_stable = |i: usize| committed_map[i].is_some() && observed_map[i].is_some();

    while base_i < base_lines.len() {
        if is_stable(base_i) {
            // Emit any side-only insertions that occur before this anchor, then
            // the stable line itself. The anchor's side positions:
            let c_anchor = committed_map[base_i].unwrap();
            let o_anchor = observed_map[base_i].unwrap();

            // Lines inserted on each side before the anchor (relative to last
            // consumed position) belong to the preceding chunk; but if we reach
            // a stable line directly we still must flush pending inserts.
            // committed pending inserts:
            let c_pending: Vec<String> = if committed_i < c_anchor {
                committed_lines[committed_i..c_anchor]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            };
            let o_pending: Vec<String> = if observed_i < o_anchor {
                observed_lines[observed_i..o_anchor]
                    .iter()
                    .map(|s| (*s).to_string())
                    .collect()
            } else {
                Vec::new()
            };
            // Resolve pending region: if both sides inserted differing content,
            // favor observed; else take whichever inserted.
            if !c_pending.is_empty() && !o_pending.is_empty() {
                if committed_cmp_lines[committed_i..c_anchor]
                    == observed_cmp_lines[observed_i..o_anchor]
                {
                    result.extend(c_pending);
                } else {
                    result.extend(o_pending);
                }
            } else if !c_pending.is_empty() {
                result.extend(c_pending);
            } else if !o_pending.is_empty() {
                result.extend(o_pending);
            }

            result.push(base_lines[base_i].to_string());
            committed_i = c_anchor + 1;
            observed_i = o_anchor + 1;
            base_i += 1;
        } else {
            // Start of a change chunk: advance base over all non-stable lines.
            let chunk_base_start = base_i;
            while base_i < base_lines.len() && !is_stable(base_i) {
                base_i += 1;
            }
            // The next stable anchor (or end) bounds how far each side consumes.
            let (c_anchor, o_anchor) = if base_i < base_lines.len() {
                (
                    committed_map[base_i].unwrap(),
                    observed_map[base_i].unwrap(),
                )
            } else {
                (committed_lines.len(), observed_lines.len())
            };

            let committed_chunk: Vec<String> = committed_lines[committed_i..c_anchor]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            let observed_chunk: Vec<String> = observed_lines[observed_i..o_anchor]
                .iter()
                .map(|s| (*s).to_string())
                .collect();

            // Determine which sides changed this base region relative to base.
            let base_cmp_chunk = &base_cmp_lines[chunk_base_start..base_i];
            let committed_cmp_chunk = &committed_cmp_lines[committed_i..c_anchor];
            let observed_cmp_chunk = &observed_cmp_lines[observed_i..o_anchor];
            let committed_changed = committed_cmp_chunk != base_cmp_chunk;
            let observed_changed = observed_cmp_chunk != base_cmp_chunk;
            let base_chunk: Vec<String> = base_lines[chunk_base_start..base_i]
                .iter()
                .map(|s| (*s).to_string())
                .collect();

            match (committed_changed, observed_changed) {
                (true, false) => result.extend(committed_chunk),
                (false, true) => result.extend(observed_chunk),
                (true, true) => {
                    // Two-sided change → favor observed (matches --theirs),
                    // unless both produced identical content.
                    if committed_cmp_chunk == observed_cmp_chunk {
                        result.extend(committed_chunk);
                    } else {
                        result.extend(observed_chunk);
                    }
                }
                (false, false) => result.extend(base_chunk),
            }

            committed_i = c_anchor;
            observed_i = o_anchor;
        }
    }

    // Flush any trailing inserts past the last base line on each side.
    let c_tail: Vec<String> = committed_lines[committed_i..]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    let o_tail: Vec<String> = observed_lines[observed_i..]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    if !c_tail.is_empty() && !o_tail.is_empty() {
        if committed_cmp_lines[committed_i..] == observed_cmp_lines[observed_i..] {
            result.extend(c_tail);
        } else {
            result.extend(o_tail);
        }
    } else if !c_tail.is_empty() {
        result.extend(c_tail);
    } else if !o_tail.is_empty() {
        result.extend(o_tail);
    }

    result.concat()
}

pub(super) fn mapped_line_range(
    base_to_target: &[Option<usize>],
    old_index: usize,
    old_len: usize,
) -> Option<(usize, usize)> {
    if old_len == 0 {
        return mapped_insertion_point(base_to_target, old_index);
    }
    let first = base_to_target.get(old_index).copied().flatten()?;
    for offset in 0..old_len {
        if base_to_target.get(old_index + offset).copied().flatten()? != first + offset {
            return None;
        }
    }
    Some((first, first + old_len))
}

pub(super) fn mapped_conflict_range(
    target_changes: &[(usize, usize, usize, usize)],
    old_index: usize,
    old_len: usize,
) -> Option<(usize, usize)> {
    if old_len == 0 {
        return None;
    }
    let old_end = old_index.saturating_add(old_len);
    let mut target_start = usize::MAX;
    let mut target_end = 0usize;
    for (change_old_start, change_old_end, change_target_start, change_target_end) in target_changes
    {
        if *change_old_start < old_end && old_index < *change_old_end {
            target_start = target_start.min(*change_target_start);
            target_end = target_end.max(*change_target_end);
        }
    }
    if target_start == usize::MAX {
        None
    } else {
        Some((target_start, target_end))
    }
}

pub(super) fn mapped_insertion_point(
    base_to_target: &[Option<usize>],
    old_index: usize,
) -> Option<(usize, usize)> {
    if base_to_target.is_empty() {
        return Some((0, 0));
    }
    if old_index > 0
        && let Some(Some(previous)) = base_to_target.get(old_index - 1)
    {
        let point = previous + 1;
        return Some((point, point));
    }
    if let Some(Some(next)) = base_to_target.get(old_index) {
        return Some((*next, *next));
    }
    None
}

fn line_without_terminator(line: &str) -> &str {
    let line = line.strip_suffix('\n').unwrap_or(line);
    line.strip_suffix('\r').unwrap_or(line)
}

pub(super) fn fill_line_ending_only_mappings(
    base_lines: &[&str],
    target_lines: &[&str],
    base_to_target: &mut [Option<usize>],
) {
    let mut used_targets = vec![false; target_lines.len()];
    for target_index in base_to_target.iter().flatten() {
        if let Some(used) = used_targets.get_mut(*target_index) {
            *used = true;
        }
    }

    let mut search_start = 0usize;
    for (base_index, base_line) in base_lines.iter().enumerate() {
        if let Some(target_index) = base_to_target[base_index] {
            search_start = search_start.max(target_index.saturating_add(1));
            continue;
        }

        let base_text = line_without_terminator(base_line);
        if let Some(target_index) = (search_start..target_lines.len()).find(|target_index| {
            !used_targets[*target_index]
                && line_without_terminator(target_lines[*target_index]) == base_text
        }) {
            base_to_target[base_index] = Some(target_index);
            used_targets[target_index] = true;
            search_start = target_index.saturating_add(1);
        }
    }
}

pub(crate) fn checkout_merge_rebased_content(
    base_content: &str,
    target_content: &str,
    observed_content: &str,
) -> String {
    if base_content == target_content {
        return observed_content.to_string();
    }
    if base_content == observed_content {
        return target_content.to_string();
    }

    let base_lines = split_lines_with_terminators(base_content);
    let target_lines = split_lines_with_terminators(target_content);
    let observed_lines = split_lines_with_terminators(observed_content);

    let mut base_to_target = vec![None; base_lines.len()];
    let mut target_changes = Vec::new();
    for op in crate::model::imara_diff_utils::capture_diff_slices(&base_lines, &target_lines) {
        match op {
            crate::model::imara_diff_utils::DiffOp::Equal {
                old_index,
                new_index,
                len,
            } => {
                for offset in 0..len {
                    base_to_target[old_index + offset] = Some(new_index + offset);
                }
            }
            crate::model::imara_diff_utils::DiffOp::Delete {
                old_index,
                old_len,
                new_index,
            } => {
                target_changes.push((old_index, old_index + old_len, new_index, new_index));
            }
            crate::model::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                target_changes.push((
                    old_index,
                    old_index + old_len,
                    new_index,
                    new_index + new_len,
                ));
            }
            crate::model::imara_diff_utils::DiffOp::Insert { .. } => {}
        }
    }
    fill_line_ending_only_mappings(&base_lines, &target_lines, &mut base_to_target);

    let mut edits = Vec::<(usize, usize, Vec<String>)>::new();
    for op in crate::model::imara_diff_utils::capture_diff_slices(&base_lines, &observed_lines) {
        match op {
            crate::model::imara_diff_utils::DiffOp::Equal { .. } => {}
            crate::model::imara_diff_utils::DiffOp::Insert {
                old_index,
                new_index,
                new_len,
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, 0) {
                    edits.push((
                        start,
                        end,
                        observed_lines[new_index..new_index + new_len]
                            .iter()
                            .map(|line| (*line).to_string())
                            .collect(),
                    ));
                }
            }
            crate::model::imara_diff_utils::DiffOp::Delete {
                old_index, old_len, ..
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, old_len)
                    .or_else(|| mapped_conflict_range(&target_changes, old_index, old_len))
                {
                    edits.push((start, end, Vec::new()));
                }
            }
            crate::model::imara_diff_utils::DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
            } => {
                if let Some((start, end)) = mapped_line_range(&base_to_target, old_index, old_len)
                    .or_else(|| mapped_conflict_range(&target_changes, old_index, old_len))
                {
                    edits.push((
                        start,
                        end,
                        observed_lines[new_index..new_index + new_len]
                            .iter()
                            .map(|line| (*line).to_string())
                            .collect(),
                    ));
                }
            }
        }
    }

    edits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.cmp(&a.1)));
    let mut rebased = target_lines
        .iter()
        .map(|line| (*line).to_string())
        .collect::<Vec<_>>();
    for (start, end, replacement) in edits {
        if start <= end && end <= rebased.len() {
            rebased.splice(start..end, replacement);
        }
    }
    rebased.concat()
}
