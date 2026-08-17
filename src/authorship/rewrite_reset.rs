use crate::authorship::attribution_tracker::LineAttribution;
use crate::authorship::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::authorship::authorship_log_serialization::AuthorshipLog;
use crate::authorship::hunk_shift::{DiffHunk, apply_hunk_shifts_to_line_attributions};
use crate::authorship::rewrite::compute_diff_trees_batch;
use crate::error::GitAiError;
use crate::git::notes_api;
use crate::git::repository::{Repository, batch_read_paths_at_treeishes};
use std::collections::HashMap;

/// Handles working log reconstruction after a backward reset (e.g. git reset --mixed HEAD~N).
///
/// After reset, HEAD is at new_tip but working tree still has content from old_tip.
/// We need to reconstruct working log entries from the authorship notes of the
/// "un-done" commits so that the next commit preserves AI attribution.
pub fn reconstruct_working_log_after_backward_reset(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
) -> Result<(), GitAiError> {
    // Pending checkpoints are normally keyed by the command's pre-move HEAD.
    // Move them before reconstructing attribution from the commits being
    // unwound. The destination can already contain a log (for example after a
    // branch was previously checked out), and rename_working_log deliberately
    // merges in that case. Doing this up front also preserves pending edits
    // when the unwound commits have no usable authorship notes.
    repo.storage.rename_working_log(old_tip, new_tip)?;

    // List all commits being "un-done" (between new_tip exclusive and old_tip inclusive)
    let commits = list_commits_in_range(repo, new_tip, old_tip);
    if commits.is_empty() {
        return Ok(());
    }

    // Read authorship notes for all un-done commits
    let mut commit_logs: Vec<(String, AuthorshipLog)> = Vec::new();
    let notes = notes_api::read_notes_batch(repo, &commits)?;
    for commit_sha in &commits {
        let Some(raw_note) = notes.get(commit_sha) else {
            continue;
        };
        let Ok(log) = AuthorshipLog::deserialize_from_string(raw_note) else {
            continue;
        };
        commit_logs.push((commit_sha.clone(), log));
    }

    if commit_logs.is_empty() {
        return Ok(());
    }

    // Compute diffs from each intermediate commit to old_tip so we can shift
    // line numbers into old_tip's coordinate space. Commits that ARE old_tip
    // need no shift.
    let diff_pairs: Vec<(String, String)> = commit_logs
        .iter()
        .filter(|(sha, _)| sha != old_tip)
        .map(|(sha, _)| (sha.clone(), old_tip.to_string()))
        .collect();

    let diff_results = if !diff_pairs.is_empty() {
        compute_diff_trees_batch(repo, &diff_pairs)?
    } else {
        Vec::new()
    };

    // Build a lookup from commit SHA to its diff result index
    let diff_idx_by_sha: HashMap<&str, usize> = diff_pairs
        .iter()
        .enumerate()
        .map(|(idx, (sha, _))| (sha.as_str(), idx))
        .collect();

    // Collect attributions from all commits, shifting intermediate ones to old_tip's
    // coordinate space. Process in chronological order (oldest first) so that later
    // commits' attributions override earlier ones for overlapping lines.
    let mut file_attributions: HashMap<String, Vec<LineAttribution>> = HashMap::new();
    let mut prompts: HashMap<String, PromptRecord> = HashMap::new();
    let mut sessions: std::collections::BTreeMap<String, SessionRecord> =
        std::collections::BTreeMap::new();
    let mut humans: std::collections::BTreeMap<String, HumanRecord> =
        std::collections::BTreeMap::new();

    for (commit_sha, log) in &commit_logs {
        let hunks_by_file: Option<&HashMap<String, Vec<DiffHunk>>> = diff_idx_by_sha
            .get(commit_sha.as_str())
            .map(|&idx| &diff_results[idx].hunks_by_file);

        extract_attributions_from_log_shifted(
            log,
            hunks_by_file,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );
    }

    if file_attributions.is_empty() {
        return Ok(());
    }

    // Use the content from old_tip (the commit being reset FROM) as the blob snapshot.
    // After a mixed/soft reset, the working tree originally had old_tip's content.
    // We cannot read the working directory here because by the time the daemon processes
    // the reset event, the user may have already modified files further.
    let mut file_blobs: HashMap<String, String> = HashMap::new();
    let mut blob_requests = Vec::new();
    for file_path in file_attributions.keys() {
        blob_requests.push((old_tip.to_string(), file_path.clone()));
        blob_requests.push((new_tip.to_string(), file_path.clone()));
    }
    let tree_contents = batch_read_paths_at_treeishes(repo, &blob_requests)?;
    for file_path in file_attributions.keys() {
        let old_key = (old_tip.to_string(), file_path.clone());
        let Some(content) = tree_contents.get(&old_key) else {
            continue;
        };
        if content.is_empty() {
            continue;
        }

        let new_key = (new_tip.to_string(), file_path.clone());
        if tree_contents.get(&new_key) != Some(content) {
            file_blobs.insert(file_path.clone(), content.clone());
        }
    }

    // If no files differ from the target (reset --hard), nothing to reconstruct
    if file_blobs.is_empty() {
        let _ = repo.storage.delete_working_log_for_base_commit(old_tip);
        return Ok(());
    }

    // Only keep attributions for files that have uncommitted content
    file_attributions.retain(|path, _| file_blobs.contains_key(path));

    // Write as INITIAL working log for new_tip.
    // Do NOT call reset_working_log() here: checkpoints may have already been
    // written between the time the reset happened and when the daemon processes
    // this event. Clearing checkpoints.jsonl would lose that data.
    let working_log = repo.storage.working_log_for_base_commit(new_tip)?;
    let mut carried = working_log.read_initial_attributions();

    working_log.write_initial_attributions_with_contents(
        file_attributions,
        prompts,
        humans,
        file_blobs,
        sessions,
    )?;

    // rename_working_log may have carried an existing INITIAL state to the
    // destination. It describes newer uncommitted work and therefore wins for
    // overlapping paths; reconstruction only fills paths that were absent.
    let reconstructed = working_log.read_initial_attributions();
    for (path, attrs) in reconstructed.files {
        carried.files.entry(path).or_insert(attrs);
    }
    for (path, blob) in reconstructed.file_blobs {
        carried.file_blobs.entry(path).or_insert(blob);
    }
    for (id, prompt) in reconstructed.prompts {
        carried.prompts.entry(id).or_insert(prompt);
    }
    for (id, human) in reconstructed.humans {
        carried.humans.entry(id).or_insert(human);
    }
    for (id, session) in reconstructed.sessions {
        carried.sessions.entry(id).or_insert(session);
    }
    working_log.write_initial(carried)?;

    // Delete old working log if it exists
    let _ = repo.storage.delete_working_log_for_base_commit(old_tip);

    Ok(())
}

fn extract_attributions_from_log_shifted(
    log: &AuthorshipLog,
    hunks_by_file: Option<&HashMap<String, Vec<DiffHunk>>>,
    file_attributions: &mut HashMap<String, Vec<LineAttribution>>,
    prompts: &mut HashMap<String, PromptRecord>,
    sessions: &mut std::collections::BTreeMap<String, SessionRecord>,
    humans: &mut std::collections::BTreeMap<String, HumanRecord>,
) {
    for fa in &log.attestations {
        let mut raw_attrs: Vec<LineAttribution> = Vec::new();
        for entry in &fa.entries {
            for range in &entry.line_ranges {
                let (start, end) = match range {
                    LineRange::Single(l) => (*l, *l),
                    LineRange::Range(s, e) => (*s, *e),
                };
                raw_attrs.push(LineAttribution::new(start, end, entry.hash.clone(), None));
            }
        }

        // Shift line numbers to old_tip's coordinate space if we have hunks for this file
        let shifted = if let Some(all_hunks) = hunks_by_file
            && let Some(file_hunks) = all_hunks.get(&fa.file_path)
            && !file_hunks.is_empty()
        {
            apply_hunk_shifts_to_line_attributions(&raw_attrs, file_hunks)
        } else {
            raw_attrs
        };

        // Merge into accumulated attributions. Later commits override earlier ones
        // for overlapping line ranges.
        let existing = file_attributions.entry(fa.file_path.clone()).or_default();
        for new_attr in shifted {
            // Remove any existing attributions that are fully covered by this new one
            existing.retain(|old| {
                !(old.start_line >= new_attr.start_line && old.end_line <= new_attr.end_line)
            });
            // For partial overlaps, trim existing attributions. The head and
            // tail trims are INDEPENDENT: when `old` strictly encloses
            // `new_attr` (old.start < new.start AND old.end > new.end) both
            // fragments must survive, so we must not `return false` after the
            // head trim alone -- that would drop the tail [new.end+1, old.end].
            let mut trimmed: Vec<LineAttribution> = Vec::new();
            existing.retain(|old| {
                let head_overlap =
                    old.start_line < new_attr.start_line && old.end_line >= new_attr.start_line;
                let tail_overlap =
                    old.end_line > new_attr.end_line && old.start_line <= new_attr.end_line;

                if !head_overlap && !tail_overlap {
                    // No partial overlap with this `old`; keep it untouched.
                    return true;
                }

                if head_overlap {
                    // Overlap at the end of old — keep old's head before new.
                    trimmed.push(LineAttribution::new(
                        old.start_line,
                        new_attr.start_line - 1,
                        old.author_id.clone(),
                        old.overrode.clone(),
                    ));
                }
                if tail_overlap {
                    // Overlap at the start of old — keep old's tail after new.
                    trimmed.push(LineAttribution::new(
                        new_attr.end_line + 1,
                        old.end_line,
                        old.author_id.clone(),
                        old.overrode.clone(),
                    ));
                }
                // The original `old` is replaced by the fragment(s) above.
                false
            });
            existing.extend(trimmed);
            existing.push(new_attr);
        }
    }

    for (key, record) in &log.metadata.prompts {
        prompts.entry(key.clone()).or_insert_with(|| record.clone());
    }
    for (key, record) in &log.metadata.sessions {
        sessions
            .entry(key.clone())
            .or_insert_with(|| record.clone());
    }
    for (key, record) in &log.metadata.humans {
        humans.entry(key.clone()).or_insert_with(|| record.clone());
    }
}

fn list_commits_in_range(repo: &Repository, base: &str, tip: &str) -> Vec<String> {
    crate::authorship::rewrite::list_commits_in_range(repo, base, tip)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::authorship::authorship_log_serialization::{AttestationEntry, FileAttestation};

    fn log_with_single_entry(file: &str, hash: &str, start: u32, end: u32) -> AuthorshipLog {
        let mut log = AuthorshipLog::new();
        let mut fa = FileAttestation::new(file.to_string());
        fa.add_entry(AttestationEntry::new(
            hash.to_string(),
            vec![LineRange::Range(start, end)],
        ));
        log.attestations.push(fa);
        log
    }

    /// Regression (#2): when a later commit's range is strictly enclosed by an
    /// earlier commit's range for the same file, BOTH the head fragment
    /// [old.start, new.start-1] and the tail fragment [new.end+1, old.end] must
    /// survive. The old code's two trim branches were mutually exclusive (the
    /// head branch `return false`d before the tail branch could run), so the
    /// tail was silently dropped.
    #[test]
    fn test_enclosed_range_preserves_head_and_tail() {
        let mut file_attributions: HashMap<String, Vec<LineAttribution>> = HashMap::new();
        let mut prompts = HashMap::new();
        let mut sessions = std::collections::BTreeMap::new();
        let mut humans = std::collections::BTreeMap::new();

        // Oldest commit first: human owns lines 1..=10 of f.txt.
        let old_log = log_with_single_entry("f.txt", "h_old", 1, 10);
        extract_attributions_from_log_shifted(
            &old_log,
            None,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );

        // Later commit: AI owns lines 4..=6 (strictly inside the human range).
        let new_log = log_with_single_entry("f.txt", "ai_new", 4, 6);
        extract_attributions_from_log_shifted(
            &new_log,
            None,
            &mut file_attributions,
            &mut prompts,
            &mut sessions,
            &mut humans,
        );

        let mut attrs = file_attributions.remove("f.txt").expect("f.txt present");
        attrs.sort_by_key(|a| a.start_line);

        // Expect three segments: human head [1,3], AI [4,6], human tail [7,10].
        assert_eq!(
            attrs.len(),
            3,
            "enclosed AI range must split the human range into head + tail, got: {:?}",
            attrs
        );
        assert_eq!((attrs[0].start_line, attrs[0].end_line), (1, 3));
        assert_eq!(attrs[0].author_id, "h_old");
        assert_eq!((attrs[1].start_line, attrs[1].end_line), (4, 6));
        assert_eq!(attrs[1].author_id, "ai_new");
        assert_eq!((attrs[2].start_line, attrs[2].end_line), (7, 10));
        assert_eq!(attrs[2].author_id, "h_old");
    }
}
