use super::attestation::committed_hunks_from_diff_result;
use super::carryover_snapshot::build_carryover_snapshot;
use super::diff_utils::{
    collect_committed_hunks, collect_unstaged_hunks, collect_unstaged_hunks_from_snapshot,
    detect_renames_in_commit,
};
use super::types::VirtualAttributions;
use crate::error::GitAiError;
use crate::model::attribution_tracker::LineAttribution;
use crate::model::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::model::authorship_log_serialization::{AttestationEntry, AuthorshipLog};
use crate::model::working_log::CheckpointKind;
use crate::operations::git::repository::Repository;
use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap, HashSet};

/// Output of the pre-loop hunk collection phase.
pub(super) struct CommitAndWorkdirHunks {
    pub committed: HashMap<String, Vec<LineRange>>,
    /// Present only when a `final_state_snapshot` was supplied.
    pub carryover_snapshot: Option<HashMap<String, String>>,
    pub unstaged: HashMap<String, Vec<LineRange>>,
    pub pure_insertions: HashMap<String, Vec<LineRange>>,
}

// ── free helpers ──────────────────────────────────────────────────────────────

/// Build the rename map (old_path → new_path) for a single commit.
///
/// When a precomputed diff is available its rename pairs are used directly.
/// Otherwise `detect_renames_in_commit` is called; errors are swallowed
/// (unwrap_or_default) so a rename-detection failure only loses rename
/// awareness rather than aborting the whole note.
pub(super) fn resolve_rename_map(
    repo: &Repository,
    parent_sha: &str,
    fallback_diff_base: &str,
    precomputed: Option<&crate::operations::authorship::rewrite::DiffTreeResult>,
    commit_sha: &str,
) -> HashMap<String, String> {
    if let Some(diff) = precomputed {
        diff.renames.iter().cloned().collect()
    } else if parent_sha != "initial" {
        detect_renames_in_commit(repo, fallback_diff_base, commit_sha).unwrap_or_default()
    } else {
        HashMap::new()
    }
}

/// Extend `pathspecs` with the renamed-to paths so downstream diff commands
/// don't filter them out.  Returns `Cow::Borrowed` when no extension is needed.
pub(super) fn extend_pathspecs_for_renames<'a>(
    pathspecs: Option<&'a HashSet<String>>,
    rename_map: &HashMap<String, String>,
) -> Option<Cow<'a, HashSet<String>>> {
    if rename_map.is_empty() {
        return pathspecs.map(Cow::Borrowed);
    }
    let ps_ref = pathspecs?;
    let mut ps = ps_ref.clone();
    for (old_path, new_path) in rename_map {
        if ps.contains(old_path) {
            ps.insert(new_path.clone());
        }
    }
    Some(Cow::Owned(ps))
}

/// Collect all hunk data needed before the per-file attribution loop.
///
/// All git spawns happen here, before any per-file or per-line work.
pub(super) fn collect_commit_and_workdir_hunks(
    repo: &Repository,
    parent_sha: &str,
    fallback_diff_base: &str,
    commit_sha: &str,
    effective_pathspecs: Option<&HashSet<String>>,
    final_state_snapshot: Option<&HashMap<String, String>>,
    precomputed: Option<&crate::operations::authorship::rewrite::DiffTreeResult>,
) -> Result<CommitAndWorkdirHunks, GitAiError> {
    let committed = if let Some(diff) = precomputed {
        committed_hunks_from_diff_result(diff, effective_pathspecs)
    } else {
        collect_committed_hunks(repo, fallback_diff_base, commit_sha, effective_pathspecs)?
    };

    let carryover_snapshot = if let Some(snapshot) = final_state_snapshot {
        Some(build_carryover_snapshot(
            repo,
            parent_sha,
            commit_sha,
            effective_pathspecs,
            snapshot,
        )?)
    } else {
        None
    };

    let (unstaged, pure_insertions) = if let Some(snapshot) = &carryover_snapshot {
        collect_unstaged_hunks_from_snapshot(repo, commit_sha, effective_pathspecs, snapshot)?
    } else {
        collect_unstaged_hunks(repo, commit_sha, effective_pathspecs)?
    };

    Ok(CommitAndWorkdirHunks {
        committed,
        carryover_snapshot,
        unstaged,
        pure_insertions,
    })
}

/// Remove unstaged line numbers that are already covered by committed hunks,
/// keeping pure-insertion lines even when they overlap.
pub(super) fn filter_committed_overlap_from_unstaged(
    unstaged_hunks: &mut HashMap<String, Vec<LineRange>>,
    committed_hunks: &HashMap<String, Vec<LineRange>>,
    pure_insertion_hunks: &HashMap<String, Vec<LineRange>>,
) {
    for (file_path, committed_ranges) in committed_hunks {
        if let Some(unstaged_ranges) = unstaged_hunks.get_mut(file_path) {
            let committed_lines: HashSet<u32> =
                committed_ranges.iter().flat_map(|r| r.expand()).collect();
            let pure_insertion_lines: HashSet<u32> = pure_insertion_hunks
                .get(file_path)
                .map(|ranges| ranges.iter().flat_map(|r| r.expand()).collect())
                .unwrap_or_default();

            let mut filtered: Vec<u32> = unstaged_ranges
                .iter()
                .flat_map(|r| r.expand())
                .filter(|line| {
                    !committed_lines.contains(line) || pure_insertion_lines.contains(line)
                })
                .collect();

            if filtered.is_empty() {
                unstaged_ranges.clear();
            } else {
                filtered.sort_unstable();
                filtered.dedup();
                *unstaged_ranges = LineRange::compress_lines(&filtered);
            }
        }
    }
    unstaged_hunks.retain(|_, ranges| !ranges.is_empty());
}

/// Fill attribution gaps inside committed hunks caused by imara_diff Equal matches.
///
/// Uses neighbor-based filling (same AI author on both sides) and a
/// content-based fallback. The two-phase borrow is encapsulated here:
/// immutable scan builds `gap_fills`, then `committed_lines_map` is mutated.
pub(super) fn fill_committed_gaps(
    committed_lines_map: &mut HashMap<String, Vec<u32>>,
    hunks: &[LineRange],
    file_content: Option<&str>,
) {
    let mut line_to_author: Vec<(u32, &str)> = Vec::new();
    for (author_id, lines) in &*committed_lines_map {
        for &line in lines {
            line_to_author.push((line, author_id.as_str()));
        }
    }
    line_to_author.sort_by_key(|(line, _)| *line);

    let gap_file_lines: Vec<&str> = file_content
        .map(|c| c.lines().collect())
        .unwrap_or_default();

    let mut content_to_ai_author: HashMap<&str, &str> = HashMap::new();
    if !gap_file_lines.is_empty() {
        for &(line_num, author) in &line_to_author {
            if !author.starts_with("h_")
                && author != CheckpointKind::Human.to_str()
                && let Some(&content) = gap_file_lines.get((line_num - 1) as usize)
                && !content.trim().is_empty()
            {
                content_to_ai_author.insert(content, author);
            }
        }
    }

    let mut gap_fills: Vec<(String, u32)> = Vec::new();
    for hunk in hunks {
        for line in hunk.expand() {
            if line_to_author
                .binary_search_by_key(&line, |(l, _)| *l)
                .is_ok()
            {
                continue;
            }

            let prev = line_to_author.iter().rev().find(|(l, _)| *l < line);
            let next = line_to_author.iter().find(|(l, _)| *l > line);

            if let (Some((_, prev_author)), Some((_, next_author))) = (prev, next)
                && prev_author == next_author
                && !prev_author.starts_with("h_")
            {
                gap_fills.push((prev_author.to_string(), line));
            } else if let Some(&content) = gap_file_lines.get((line - 1) as usize)
                && let Some(&author) = content_to_ai_author.get(content)
            {
                gap_fills.push((author.to_string(), line));
            }
        }
    }

    for (author_id, line) in gap_fills {
        committed_lines_map.entry(author_id).or_default().push(line);
    }
}

/// Emit attestation entries from `committed_lines_map` into `authorship_log`.
///
/// Skips the legacy "human" sentinel.  Uses `LineRange::compress_lines` for
/// range encoding.  `attestation_path` must be pre-computed by the caller as
/// `rename_map.get(&nfc_file_path).unwrap_or(&nfc_file_path)`.
pub(super) fn append_committed_attestations(
    authorship_log: &mut AuthorshipLog,
    committed_lines_map: HashMap<String, Vec<u32>>,
    attestation_path: &str,
) {
    for (author_id, mut lines) in committed_lines_map {
        if author_id == CheckpointKind::Human.to_str() {
            continue;
        }
        lines.sort();
        lines.dedup();
        if lines.is_empty() {
            continue;
        }
        let ranges = LineRange::compress_lines(&lines);
        let entry = AttestationEntry::new(author_id, ranges);
        authorship_log
            .get_or_create_file(attestation_path)
            .add_entry(entry);
    }
}

// ── VirtualAttributions methods ───────────────────────────────────────────────

impl VirtualAttributions {
    /// Convert `uncommitted_lines_map` into `LineAttribution` entries, populating
    /// `initial_humans` and `initial_sessions` for any h_/s_ authors encountered.
    ///
    /// Skips the legacy "human" sentinel.  Uses `LineRange::compress_lines`.
    pub(super) fn build_uncommitted_initial_attrs(
        &self,
        uncommitted_lines_map: HashMap<String, Vec<u32>>,
        initial_humans: &mut BTreeMap<String, HumanRecord>,
        initial_sessions: &mut BTreeMap<String, SessionRecord>,
    ) -> Vec<LineAttribution> {
        let mut attrs: Vec<LineAttribution> = Vec::new();
        for (author_id, mut lines) in uncommitted_lines_map {
            if author_id == CheckpointKind::Human.to_str() {
                continue;
            }
            lines.sort();
            lines.dedup();
            if lines.is_empty() {
                continue;
            }

            if author_id.starts_with("h_")
                && let Some(record) = self.humans.get(&author_id)
            {
                initial_humans.insert(author_id.clone(), record.clone());
            }

            if author_id.starts_with("s_") {
                let session_key = author_id
                    .split("::")
                    .next()
                    .unwrap_or(&author_id)
                    .to_string();
                if let Some(record) = self.sessions.get(&session_key) {
                    initial_sessions.insert(session_key, record.clone());
                }
            }

            for range in LineRange::compress_lines(&lines) {
                let (start_line, end_line) = range.inclusive_bounds();
                attrs.push(LineAttribution {
                    start_line,
                    end_line,
                    author_id: author_id.clone(),
                    overrode: None,
                });
            }
        }
        attrs
    }

    /// Record the initial file content for the working-log.
    ///
    /// Lookup order follows the NFC/raw asymmetry documented in the module:
    /// snapshot uses `initial_path`-then-raw; `self.file_contents` uses raw-then-NFC.
    pub(super) fn record_initial_file(
        &self,
        initial_path: &str,
        file_path: &str,
        nfc_file_path: &str,
        carryover_snapshot: Option<&HashMap<String, String>>,
        initial_file_contents: &mut HashMap<String, String>,
    ) {
        if let Some(snapshot) = carryover_snapshot {
            if let Some(content) = snapshot
                .get(initial_path)
                .or_else(|| snapshot.get(file_path))
            {
                initial_file_contents.insert(initial_path.to_owned(), content.clone());
            }
        } else if let Some(content) = self
            .file_contents
            .get(file_path)
            .or_else(|| self.file_contents.get(nfc_file_path))
        {
            initial_file_contents.insert(initial_path.to_owned(), content.clone());
        }
    }

    /// Prune INITIAL-only prompts with no committed lines, and sessions with no
    /// attestation entries.
    pub(super) fn prune_unreferenced_metadata(&self, authorship_log: &mut AuthorshipLog) {
        if !self.initial_only_prompt_ids.is_empty() {
            let committed_prompt_ids: HashSet<&String> = authorship_log
                .attestations
                .iter()
                .flat_map(|file_att| file_att.entries.iter())
                .map(|entry| &entry.hash)
                .collect();
            authorship_log.metadata.prompts.retain(|prompt_id, _| {
                !self.initial_only_prompt_ids.contains(prompt_id)
                    || committed_prompt_ids.contains(prompt_id)
            });
        }

        let committed_session_ids: HashSet<String> = authorship_log
            .attestations
            .iter()
            .flat_map(|file_att| file_att.entries.iter())
            .filter_map(|entry| {
                if entry.hash.starts_with("s_") {
                    Some(
                        entry
                            .hash
                            .split("::")
                            .next()
                            .unwrap_or(&entry.hash)
                            .to_string(),
                    )
                } else {
                    None
                }
            })
            .collect();
        authorship_log
            .metadata
            .sessions
            .retain(|session_id, _| committed_session_ids.contains(session_id));
    }

    /// Build the initial-prompts map from uncommitted referenced prompt IDs.
    pub(super) fn build_initial_prompts(
        &self,
        referenced_prompts: HashSet<String>,
    ) -> HashMap<String, PromptRecord> {
        referenced_prompts
            .into_iter()
            .filter_map(|prompt_id| {
                let prompt = self.prompts.get(&prompt_id)?.values().next()?;
                Some((prompt_id, prompt.clone()))
            })
            .collect()
    }
}
