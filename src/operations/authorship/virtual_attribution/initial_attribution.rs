use super::diff_utils::collect_committed_hunks;
use super::types::VirtualAttributions;
use crate::error::GitAiError;
use crate::model::attribution_tracker::LineAttribution;
use crate::model::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::model::working_log::{CheckpointKind, InitialAttributions};
use crate::operations::git::repository::Repository;
use std::collections::{BTreeMap, HashMap, HashSet};
use unicode_normalization::UnicodeNormalization;

impl VirtualAttributions {
    /// Convert VirtualAttributions to AuthorshipLog only (index-only mode)
    ///
    /// This is a simplified version of `to_authorship_log_and_initial_working_log` that:
    /// - Only returns an AuthorshipLog (no InitialAttributions)
    /// - Doesn't check the working copy or unstaged hunks
    /// - Is used for commits that have already landed
    ///
    /// This is useful for retroactively generating authorship logs from working logs
    /// where we know the commit has landed and don't care about uncommitted work.
    // only being used by stats-delta in a fork
    #[allow(dead_code)]
    pub fn to_authorship_log_index_only(
        &self,
        repo: &Repository,
        parent_sha: &str,
        commit_sha: &str,
        pathspecs: Option<&HashSet<String>>,
    ) -> Result<crate::model::authorship_log_serialization::AuthorshipLog, GitAiError> {
        use std::collections::HashMap as StdHashMap;

        let mut authorship_log = self.authorship_log_with_metadata();

        // Get committed hunks only (no need to check working copy)
        let committed_hunks = collect_committed_hunks(repo, parent_sha, commit_sha, pathspecs)?;

        // Process each file
        for (file_path, (_, line_attrs)) in &self.attributions {
            if line_attrs.is_empty() {
                continue;
            }

            // Get the committed hunks for this file (if any).
            // NFC-normalise the key (see first loop's comment for rationale).
            let nfc_file_path: String = file_path.nfc().collect();
            let file_committed_hunks = match committed_hunks.get(&nfc_file_path) {
                Some(hunks) => hunks,
                None => continue, // No committed hunks for this file, skip
            };

            // Map author_id -> line numbers (in commit coordinates)
            let mut committed_lines_map: StdHashMap<String, Vec<u32>> = StdHashMap::new();

            for line_attr in line_attrs {
                // Since we're not dealing with unstaged hunks, the line numbers in VirtualAttributions
                // are already in the right coordinates (working log coordinates = commit coordinates)
                for line_num in line_attr.start_line..=line_attr.end_line {
                    // Check if this line is in any committed hunk
                    let is_committed = file_committed_hunks
                        .iter()
                        .any(|hunk| hunk.contains(line_num));

                    if is_committed {
                        committed_lines_map
                            .entry(line_attr.author_id.clone())
                            .or_default()
                            .push(line_num);
                    }
                }
            }

            // Fill attribution gaps for lines in committed hunks that weren't
            // directly attributed (e.g. empty lines between AI-authored blocks).
            // Only fill if both nearest neighbors share the same AI author.
            {
                let mut line_to_author: Vec<(u32, &str)> = Vec::new();
                for (author_id, lines) in &committed_lines_map {
                    for &line in lines {
                        line_to_author.push((line, author_id.as_str()));
                    }
                }
                line_to_author.sort_by_key(|(line, _)| *line);

                let mut gap_fills: Vec<(String, u32)> = Vec::new();

                for hunk in file_committed_hunks {
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
                        }
                    }
                }

                for (author_id, line) in gap_fills {
                    committed_lines_map.entry(author_id).or_default().push(line);
                }
            }

            // Add committed attributions to authorship log
            if !committed_lines_map.is_empty() {
                // Create attestation entries from committed lines
                for (author_id, mut lines) in committed_lines_map {
                    // Skip the legacy "human" sentinel (CheckpointKind::Human checkpoints that were
                    // never attested). KnownHuman lines use h_-prefixed author IDs and pass through.
                    if author_id == CheckpointKind::Human.to_str() {
                        continue;
                    }

                    lines.sort();
                    lines.dedup();

                    if lines.is_empty() {
                        continue;
                    }

                    let ranges = LineRange::compress_lines(&lines);

                    let entry = crate::model::authorship_log_serialization::AttestationEntry::new(
                        author_id, ranges,
                    );

                    let file_attestation = authorship_log.get_or_create_file(&nfc_file_path);
                    file_attestation.add_entry(entry);
                }
            }
        }

        // Remove INITIAL-only prompts without committed lines (same logic as the
        // primary method — see comment there).
        if !self.initial_only_prompt_ids.is_empty() {
            let committed_prompt_ids: std::collections::HashSet<&String> = authorship_log
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

        Ok(authorship_log)
    }

    /// Convert all current AI attributions into INITIAL without consulting the live worktree.
    pub fn to_initial_working_log_only(&self) -> InitialAttributions {
        let mut initial_files: HashMap<String, Vec<LineAttribution>> = HashMap::new();
        let mut referenced_prompts = HashSet::new();

        for (file_path, (_, line_attrs)) in &self.attributions {
            let filtered: Vec<LineAttribution> = line_attrs
                .iter()
                .filter(|attr| attr.author_id != CheckpointKind::Human.to_str())
                .cloned()
                .collect();
            if filtered.is_empty() {
                continue;
            }
            for attr in &filtered {
                referenced_prompts.insert(attr.author_id.clone());
            }
            initial_files.insert(file_path.clone(), filtered);
        }

        let mut initial_prompts = HashMap::new();
        for prompt_id in &referenced_prompts {
            if let Some(commits) = self.prompts.get(prompt_id)
                && let Some(prompt) = commits.values().next()
            {
                initial_prompts.insert(prompt_id.clone(), prompt.clone());
            }
        }

        // Collect h_ human records referenced by retained attributions
        let mut initial_humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
        for author_id in &referenced_prompts {
            if author_id.starts_with("h_")
                && let Some(record) = self.humans.get(author_id)
            {
                initial_humans.insert(author_id.clone(), record.clone());
            }
        }

        // Collect s_ session records referenced by retained attributions
        let mut initial_sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
        for author_id in &referenced_prompts {
            if author_id.starts_with("s_") {
                let session_key = author_id
                    .split("::")
                    .next()
                    .unwrap_or(author_id)
                    .to_string();
                if let Some(record) = self.sessions.get(&session_key) {
                    initial_sessions.insert(session_key, record.clone());
                }
            }
        }

        InitialAttributions {
            files: initial_files,
            prompts: initial_prompts,
            file_blobs: HashMap::new(),
            humans: initial_humans,
            sessions: initial_sessions,
        }
    }

    /// Calculate and update prompt metrics (accepted_lines, overridden_lines, total_additions, total_deletions)
    pub fn calculate_and_update_prompt_metrics(
        prompts: &mut BTreeMap<String, BTreeMap<String, PromptRecord>>,
        attributions: &HashMap<
            String,
            (
                Vec<crate::model::attribution_tracker::Attribution>,
                Vec<LineAttribution>,
            ),
        >,
        session_additions: &HashMap<String, u32>,
        session_deletions: &HashMap<String, u32>,
    ) {
        use std::collections::HashSet;

        // Collect all line attributions
        let all_line_attributions: Vec<&LineAttribution> = attributions
            .values()
            .flat_map(|(_, line_attrs)| line_attrs.iter())
            .collect();

        // Calculate accepted_lines: count lines in final attributions per session
        let mut session_accepted_lines: HashMap<String, u32> = HashMap::new();
        for (_char_attrs, line_attrs) in attributions.values() {
            for line_attr in line_attrs {
                // Skip human attributions - we only track AI prompt metrics
                if line_attr.author_id == CheckpointKind::Human.to_str() {
                    continue;
                }

                let line_count = line_attr.end_line - line_attr.start_line + 1;
                *session_accepted_lines
                    .entry(line_attr.author_id.clone())
                    .or_insert(0) += line_count;
            }
        }

        // Calculate overridden_lines: count lines where overrode field matches session_id
        // NOTE: We intentionally include human attributions here because when a human
        // overrides an AI line, the attribution has author_id="human" and overrode="ai_prompt_id"
        let mut session_overridden_lines: HashMap<String, u32> = HashMap::new();
        for line_attr in &all_line_attributions {
            if let Some(overrode_id) = &line_attr.overrode {
                let mut overridden_lines: HashSet<u32> = HashSet::new();
                for line in line_attr.start_line..=line_attr.end_line {
                    overridden_lines.insert(line);
                }
                *session_overridden_lines
                    .entry(overrode_id.clone())
                    .or_insert(0) += overridden_lines.len() as u32;
            }
        }

        // Update all prompt records with calculated metrics
        for (session_id, commits) in prompts.iter_mut() {
            for prompt_record in commits.values_mut() {
                prompt_record.total_additions = *session_additions.get(session_id).unwrap_or(&0);
                prompt_record.total_deletions = *session_deletions.get(session_id).unwrap_or(&0);
                prompt_record.accepted_lines =
                    *session_accepted_lines.get(session_id).unwrap_or(&0);
                prompt_record.overriden_lines =
                    *session_overridden_lines.get(session_id).unwrap_or(&0);
            }
        }
    }

    /// Filter prompts and attributions to only include those from specific commits
    /// This is useful for range analysis where we only want to count AI contributions
    /// from commits within the range, not from before
    pub fn filter_to_commits(&mut self, commit_shas: &HashSet<String>) {
        // Capture original AI prompt IDs before filtering
        let original_prompt_ids: HashSet<String> = self.prompts.keys().cloned().collect();

        // Filter prompts to only include those from the specified commits
        let mut filtered_prompts = BTreeMap::new();

        for (prompt_id, commits_map) in &self.prompts {
            let filtered_commits: BTreeMap<String, PromptRecord> = commits_map
                .iter()
                .filter(|(commit_sha, _)| commit_shas.contains(*commit_sha))
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();

            if !filtered_commits.is_empty() {
                filtered_prompts.insert(prompt_id.clone(), filtered_commits);
            }
        }

        self.prompts = filtered_prompts;

        // Get set of valid prompt IDs after filtering
        let valid_prompt_ids: HashSet<String> = self.prompts.keys().cloned().collect();

        // Remove attributions that reference filtered-out prompts
        for (char_attrs, _line_attrs) in self.attributions.values_mut() {
            char_attrs.retain(|attr| {
                // Keep human attributions (not in original prompts at all)
                // OR keep AI attributions that are still valid after filtering
                !original_prompt_ids.contains(&attr.author_id)
                    || valid_prompt_ids.contains(&attr.author_id)
            });
        }

        // Recalculate line attributions for all files
        for (file_path, (char_attrs, line_attrs)) in self.attributions.iter_mut() {
            let file_content = self
                .file_contents
                .get(file_path)
                .cloned()
                .unwrap_or_default();
            *line_attrs = crate::model::attribution_tracker::attributions_to_line_attributions(
                char_attrs,
                &file_content,
            );
        }
    }
}
