//! Attribution computation for diff hunks.
//!
//! This module takes parsed [`DiffHunk`] values and enriches them with
//! authorship data from `git-ai blame` or from a pre-loaded [`AuthorshipLog`].
//! The results are collected into [`DiffBuildArtifacts`].

use crate::error::GitAiError;
use crate::model::authorship_log::{HumanRecord, LineRange, PromptRecord, SessionRecord};
use crate::model::authorship_log_serialization::AuthorshipLog;
use crate::operations::authorship::ignore::{
    build_ignore_matcher, effective_ignore_patterns, should_ignore_file_with_matcher,
};
use crate::operations::commands::blame::GitAiBlameOptions;
use crate::operations::git::repository::Repository;
use std::collections::{BTreeMap, HashMap, HashSet};

use crate::operations::commands::diff::{
    Attribution, DiffBuildArtifacts, DiffCommandOptions, DiffHunk, DiffLineKey, LineSide,
};
use crate::operations::commands::diff_json_builder::{build_json_hunks, extract_session_id};
use crate::operations::commands::diff_note_attribution::build_line_attribution_from_note;
use crate::operations::commands::diff_parsing::{
    build_line_content_map, get_diff_sections_by_file,
};

// ============================================================================
// Public API
// ============================================================================

/// Overlay attribution on diff hunks, returning a per-line attribution map.
///
/// This is a convenience wrapper around [`build_diff_artifacts`] for callers
/// that only need the attribution map.
pub fn overlay_diff_attributions(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    hunks: &[DiffHunk],
) -> Result<HashMap<DiffLineKey, Attribution>, GitAiError> {
    let (_, attributions, _, _, _, _, _) = build_line_attribution_data(
        repo,
        from_commit,
        to_commit,
        hunks,
        &DiffCommandOptions::default(),
    )?;
    Ok(attributions)
}

/// Build [`DiffBuildArtifacts`] for the diff between `from_commit` and `to_commit`.
pub fn build_diff_artifacts(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    options: &DiffCommandOptions,
) -> Result<DiffBuildArtifacts, GitAiError> {
    build_diff_artifacts_with_note(repo, from_commit, to_commit, options, None)
}

/// Build [`DiffBuildArtifacts`], optionally using a pre-loaded authorship log
/// to skip the blame call.
pub fn build_diff_artifacts_with_note(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    options: &DiffCommandOptions,
    authorship_log: Option<&AuthorshipLog>,
) -> Result<DiffBuildArtifacts, GitAiError> {
    use crate::operations::commands::diff_parsing::get_diff_with_line_numbers;

    let hunks = get_diff_with_line_numbers(repo, from_commit, to_commit)?;

    if let Some(note) = authorship_log {
        return build_diff_artifacts_from_hunks(repo, hunks, to_commit, Some(note));
    }

    // Slow path: no authorship log — run blame.
    let effective_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&effective_patterns);
    let diff_sections = get_diff_sections_by_file(repo, from_commit, to_commit)?;
    let mut included_files: HashSet<String> = diff_sections
        .into_iter()
        .map(|(file_path, _)| file_path)
        .filter(|file_path| {
            !file_path.is_empty() && !should_ignore_file_with_matcher(file_path, &ignore_matcher)
        })
        .collect();

    let mut hunks = hunks;
    hunks.retain(|hunk| {
        !hunk.file_path.is_empty()
            && !should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher)
    });
    included_files.extend(hunks.iter().map(|h| h.file_path.clone()));
    let line_contents = build_line_content_map(&hunks);

    let (annotations_by_file, attributions, line_details, prompts, sessions, humans, mut commits) =
        build_line_attribution_data(repo, from_commit, to_commit, &hunks, options)?;

    let json_hunks = build_json_hunks(
        repo,
        &hunks,
        &line_details,
        &line_contents,
        to_commit,
        &mut commits,
    )?;

    Ok(DiffBuildArtifacts {
        attributions,
        annotations_by_file,
        prompts,
        sessions,
        humans,
        json_hunks,
        commits,
        included_files,
    })
}

/// Build diff artifacts from pre-computed hunks, avoiding redundant git calls.
///
/// Used by the post-commit hook path where the caller already has the hunks
/// from a single [`super::diff_parsing::get_diff_with_line_numbers`] call.
pub fn build_diff_artifacts_from_hunks(
    repo: &Repository,
    hunks: Vec<DiffHunk>,
    to_commit: &str,
    authorship_log: Option<&AuthorshipLog>,
) -> Result<DiffBuildArtifacts, GitAiError> {
    let effective_patterns = effective_ignore_patterns(repo, &[], &[]);
    let ignore_matcher = build_ignore_matcher(&effective_patterns);

    let mut hunks = hunks;
    hunks.retain(|hunk| {
        !hunk.file_path.is_empty()
            && !should_ignore_file_with_matcher(&hunk.file_path, &ignore_matcher)
    });

    let included_files: HashSet<String> = hunks.iter().map(|h| h.file_path.clone()).collect();
    let line_contents = build_line_content_map(&hunks);

    let (annotations_by_file, attributions, line_details, prompts, sessions, humans, mut commits) =
        if let Some(note) = authorship_log {
            build_line_attribution_from_note(to_commit, &hunks, note)
        } else {
            (
                BTreeMap::new(),
                HashMap::new(),
                HashMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
                BTreeMap::new(),
            )
        };

    let json_hunks = build_json_hunks(
        repo,
        &hunks,
        &line_details,
        &line_contents,
        to_commit,
        &mut commits,
    )?;

    Ok(DiffBuildArtifacts {
        attributions,
        annotations_by_file,
        prompts,
        sessions,
        humans,
        json_hunks,
        commits,
        included_files,
    })
}

// ============================================================================
// Internal attribution logic
// ============================================================================

/// Internal detail attached to each line for building JSON hunks.
#[derive(Debug, Clone)]
pub(crate) struct LineAttributionDetail {
    pub commit_sha: Option<String>,
    pub prompt_id: Option<String>,
    pub human_id: Option<String>,
}

#[allow(clippy::type_complexity)]
pub(super) fn build_line_attribution_data(
    repo: &Repository,
    from_commit: &str,
    to_commit: &str,
    hunks: &[DiffHunk],
    options: &DiffCommandOptions,
) -> Result<
    (
        BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
        HashMap<DiffLineKey, Attribution>,
        HashMap<DiffLineKey, LineAttributionDetail>,
        BTreeMap<String, PromptRecord>,
        BTreeMap<String, SessionRecord>,
        BTreeMap<String, HumanRecord>,
        BTreeMap<String, crate::operations::commands::diff::DiffCommitMetadata>,
    ),
    GitAiError,
> {
    let mut annotations_by_file: BTreeMap<String, BTreeMap<String, Vec<LineRange>>> =
        BTreeMap::new();
    let mut attributions: HashMap<DiffLineKey, Attribution> = HashMap::new();
    let mut line_details: HashMap<DiffLineKey, LineAttributionDetail> = HashMap::new();
    let mut prompts: BTreeMap<String, PromptRecord> = BTreeMap::new();
    let mut sessions: BTreeMap<String, SessionRecord> = BTreeMap::new();
    let mut humans: BTreeMap<String, HumanRecord> = BTreeMap::new();
    let mut commits: BTreeMap<String, crate::operations::commands::diff::DiffCommitMetadata> =
        BTreeMap::new();

    let added_lines_by_file = collect_lines_by_file(hunks, LineSide::New);
    for (file_path, lines) in &added_lines_by_file {
        apply_blame_for_side(
            repo,
            file_path,
            file_path,
            lines,
            LineSide::New,
            from_commit,
            Some(to_commit),
            None,
            &mut annotations_by_file,
            &mut attributions,
            &mut line_details,
            &mut prompts,
            &mut sessions,
            &mut humans,
            &mut commits,
        );
    }

    if options.blame_deletions {
        let deleted_lines_by_blame_and_result = collect_old_lines_by_blame_and_result(hunks);
        for ((blame_file_path, result_file_path), lines) in deleted_lines_by_blame_and_result {
            apply_blame_for_side(
                repo,
                &blame_file_path,
                &result_file_path,
                &lines,
                LineSide::Old,
                from_commit,
                None,
                options.blame_deletions_since.clone(),
                &mut annotations_by_file,
                &mut attributions,
                &mut line_details,
                &mut prompts,
                &mut sessions,
                &mut humans,
                &mut commits,
            );
        }
    }

    Ok((
        annotations_by_file,
        attributions,
        line_details,
        prompts,
        sessions,
        humans,
        commits,
    ))
}

#[allow(clippy::too_many_arguments)]
fn apply_blame_for_side(
    repo: &Repository,
    blame_file_path: &str,
    result_file_path: &str,
    lines: &[u32],
    side: LineSide,
    from_commit: &str,
    newest_commit: Option<&str>,
    oldest_date_spec: Option<String>,
    annotations_by_file: &mut BTreeMap<String, BTreeMap<String, Vec<LineRange>>>,
    attributions: &mut HashMap<DiffLineKey, Attribution>,
    line_details: &mut HashMap<DiffLineKey, LineAttributionDetail>,
    prompts: &mut BTreeMap<String, PromptRecord>,
    sessions: &mut BTreeMap<String, SessionRecord>,
    humans: &mut BTreeMap<String, HumanRecord>,
    commits: &mut BTreeMap<String, crate::operations::commands::diff::DiffCommitMetadata>,
) {
    if lines.is_empty() {
        return;
    }

    let line_ranges = LineRange::compress_bounds(lines);
    if line_ranges.is_empty() {
        return;
    }

    let mut blame_options = GitAiBlameOptions {
        line_ranges,
        no_output: true,
        use_prompt_hashes_as_names: true,
        newest_commit: Some(newest_commit.unwrap_or(from_commit).to_string()),
        ..GitAiBlameOptions::default()
    };
    if matches!(side, LineSide::New) {
        blame_options.oldest_commit = Some(from_commit.to_string());
    } else {
        blame_options.oldest_date_spec = oldest_date_spec;
    }

    let analysis = match repo.blame_analysis(blame_file_path, &blame_options) {
        Ok(analysis) => analysis,
        Err(_) => {
            for line in lines {
                attributions.insert(
                    DiffLineKey {
                        file: result_file_path.to_string(),
                        line: *line,
                        side: side.clone(),
                    },
                    Attribution::NoData,
                );
            }
            return;
        }
    };

    for (prompt_id, prompt_record) in &analysis.prompt_records {
        if prompt_id.starts_with("s_") {
            // Session-format attestation: look up the SessionRecord from blame analysis.
            let session_key = extract_session_id(prompt_id);
            if let Some(session_record) = analysis.session_records.get(session_key) {
                sessions
                    .entry(session_key.to_string())
                    .or_insert_with(|| session_record.clone());
            } else {
                // Fallback: convert PromptRecord back to SessionRecord.
                sessions
                    .entry(session_key.to_string())
                    .or_insert_with(|| SessionRecord {
                        agent_id: prompt_record.agent_id.clone(),
                        human_author: prompt_record.human_author.clone(),
                        custom_attributes: prompt_record.custom_attributes.clone(),
                    });
            }
        } else {
            prompts
                .entry(prompt_id.clone())
                .or_insert_with(|| prompt_record.clone());
        }
    }

    for (human_id, human_record) in &analysis.humans {
        humans
            .entry(human_id.clone())
            .or_insert_with(|| human_record.clone());
    }

    let mut line_to_commit: HashMap<u32, String> = HashMap::new();
    for blame_hunk in &analysis.blame_hunks {
        crate::operations::commands::diff_json_builder::ensure_commit_metadata(
            repo,
            &blame_hunk.commit_sha,
            commits,
        );
        for line in blame_hunk.range.0..=blame_hunk.range.1 {
            line_to_commit.insert(line, blame_hunk.commit_sha.clone());
        }
    }

    let mut lines_by_prompt_id: HashMap<String, Vec<u32>> = HashMap::new();

    for line in lines {
        let key = DiffLineKey {
            file: result_file_path.to_string(),
            line: *line,
            side: side.clone(),
        };

        if let Some(author_marker) = analysis.line_authors.get(line) {
            let prompt_id = if analysis.prompt_records.contains_key(author_marker) {
                Some(author_marker.clone())
            } else {
                None
            };

            let human_id = if author_marker.starts_with("h_") {
                Some(author_marker.clone())
            } else {
                None
            };

            let attribution = if let Some(ref id) = prompt_id {
                let tool = analysis
                    .prompt_records
                    .get(id)
                    .map(|prompt| prompt.agent_id.tool.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                lines_by_prompt_id
                    .entry(id.clone())
                    .or_default()
                    .push(*line);
                Attribution::Ai(tool)
            } else if author_marker.starts_with("h_") {
                // Known human attestation (h_-prefixed hash from KnownHuman checkpoint).
                Attribution::Human(author_marker.clone())
            } else {
                // Legacy or unrecognized marker (e.g. "human") — treat as unattested.
                Attribution::NoData
            };
            attributions.insert(key.clone(), attribution);
            line_details.insert(
                key,
                LineAttributionDetail {
                    commit_sha: line_to_commit.get(line).cloned(),
                    prompt_id,
                    human_id,
                },
            );
        } else {
            attributions.insert(key.clone(), Attribution::NoData);
            line_details.insert(
                key,
                LineAttributionDetail {
                    commit_sha: None,
                    prompt_id: None,
                    human_id: None,
                },
            );
        }
    }

    if matches!(side, LineSide::New) {
        let file_annotations = annotations_by_file
            .entry(result_file_path.to_string())
            .or_default();
        for (prompt_id, mut prompt_lines) in lines_by_prompt_id {
            prompt_lines.sort_unstable();
            prompt_lines.dedup();
            file_annotations.insert(prompt_id, LineRange::compress_lines(&prompt_lines));
        }
    }
}

// ============================================================================
// Line collection helpers
// ============================================================================

pub(crate) fn collect_lines_by_file(
    hunks: &[DiffHunk],
    side: LineSide,
) -> HashMap<String, Vec<u32>> {
    let mut lines_by_file: HashMap<String, Vec<u32>> = HashMap::new();
    for hunk in hunks {
        let lines = match side {
            LineSide::Old => &hunk.deleted_lines,
            LineSide::New => &hunk.added_lines,
        };
        if lines.is_empty() {
            continue;
        }
        let key = match side {
            LineSide::Old => hunk.old_file_path.as_deref().unwrap_or(&hunk.file_path),
            LineSide::New => &hunk.file_path,
        };
        lines_by_file
            .entry(key.to_string())
            .or_default()
            .extend(lines.iter().copied());
    }

    for lines in lines_by_file.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    lines_by_file
}

fn collect_old_lines_by_blame_and_result(
    hunks: &[DiffHunk],
) -> HashMap<(String, String), Vec<u32>> {
    let mut lines_by_file_pair: HashMap<(String, String), Vec<u32>> = HashMap::new();

    for hunk in hunks {
        if hunk.deleted_lines.is_empty() {
            continue;
        }

        let blame_file = hunk
            .old_file_path
            .clone()
            .unwrap_or_else(|| hunk.file_path.clone());
        let result_file = hunk.file_path.clone();
        lines_by_file_pair
            .entry((blame_file, result_file))
            .or_default()
            .extend(hunk.deleted_lines.iter().copied());
    }

    for lines in lines_by_file_pair.values_mut() {
        lines.sort_unstable();
        lines.dedup();
    }

    lines_by_file_pair
}
