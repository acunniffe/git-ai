use std::collections::{HashMap, HashSet};

use crate::authorship::authorship_log::LineRange;
use crate::authorship::authorship_log_serialization::{
    AttestationEntry, AuthorshipLog, FileAttestation,
};
use crate::authorship::hunk_shift::apply_hunk_shifts_to_file_attestation;
use crate::authorship::rewrite::compute_diff_trees_batch;
use crate::authorship::rewrite::{RewriteMetricCommit, RewriteMetricOperation};
use crate::error::GitAiError;
use crate::git::notes_api;
use crate::git::repository::{Repository, exec_git, exec_git_stdin};

/// One reverted commit to reconstruct: the new revert commit, its parent, and
/// the original commit that was reverted (used to locate the source note).
pub struct RevertSpec {
    pub revert_commit: String,
    pub parent: Option<String>,
    pub reverted_commit: Option<String>,
}

/// Batched revert-attribution reconstruction for one `git revert A B C ...`
/// invocation. Performs a CONSTANT number of git spawns regardless of how many
/// commits were reverted: one batched first-parent rev-parse, one batched note
/// read, one batched diff-tree (covering every commit's two diff pairs), and one
/// batched note write. All per-commit work below is pure in-memory.
pub fn handle_revert_commits(repo: &Repository, specs: &[RevertSpec]) -> Result<(), GitAiError> {
    handle_revert_commits_with_metrics(repo, specs).map(|_| ())
}

pub(crate) fn handle_revert_commits_with_metrics(
    repo: &Repository,
    specs: &[RevertSpec],
) -> Result<Vec<RewriteMetricCommit>, GitAiError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let collect_metrics = crate::authorship::rewrite::rewrite_metrics_enabled();

    // Resolve every spec's parent_sha (only those missing need a lookup) and the
    // source_base_sha (= first parent of the reverted commit, or of the parent
    // for the legacy HEAD-only path). Batch all the required `<sha>^1` /
    // `<sha>~1` resolutions into a single rev-parse.
    struct Resolved {
        revert_commit: String,
        parent_sha: String,
        source_base_sha: String,
        original_shas: Vec<String>,
    }

    // Collect the revspecs we need resolved, deduplicated.
    let mut to_resolve: Vec<String> = Vec::new();
    let push_unique = |spec: String, acc: &mut Vec<String>| {
        if !acc.contains(&spec) {
            acc.push(spec);
        }
    };
    for spec in specs {
        if spec.parent.as_deref().unwrap_or("").is_empty() {
            push_unique(format!("{}~1", spec.revert_commit), &mut to_resolve);
        }
        match spec.reverted_commit.as_deref() {
            Some(rc) if !rc.is_empty() => push_unique(format!("{}^1", rc), &mut to_resolve),
            _ => {
                // Legacy path resolves first-parent of the (resolved) parent;
                // handled after parents are known, below.
            }
        }
    }
    let resolved_revspecs = batch_rev_parse_verify(repo, &to_resolve)?;

    // Second pass: some legacy specs need first-parent of the parent_sha, which
    // itself may have just been resolved. Resolve those in a second batch.
    let mut resolved: Vec<Resolved> = Vec::new();
    let mut legacy_parent_firstparent: Vec<String> = Vec::new();
    for spec in specs {
        let parent_sha = match spec.parent.as_deref() {
            Some(p) if !p.is_empty() => p.to_string(),
            _ => resolved_revspecs
                .get(&format!("{}~1", spec.revert_commit))
                .cloned()
                .unwrap_or_default(),
        };
        if parent_sha.is_empty() {
            continue;
        }
        match spec.reverted_commit.as_deref() {
            Some(rc) if !rc.is_empty() => {
                let source_base_sha = resolved_revspecs
                    .get(&format!("{}^1", rc))
                    .cloned()
                    .unwrap_or_default();
                resolved.push(Resolved {
                    revert_commit: spec.revert_commit.clone(),
                    parent_sha,
                    source_base_sha,
                    original_shas: vec![rc.to_string()],
                });
            }
            _ => {
                push_unique(format!("{}^1", parent_sha), &mut legacy_parent_firstparent);
                resolved.push(Resolved {
                    revert_commit: spec.revert_commit.clone(),
                    parent_sha,
                    // Placeholder; filled from the second batch below.
                    source_base_sha: String::new(),
                    original_shas: Vec::new(),
                });
            }
        }
    }
    if !legacy_parent_firstparent.is_empty() {
        let legacy_resolved = batch_rev_parse_verify(repo, &legacy_parent_firstparent)?;
        for r in &mut resolved {
            if r.source_base_sha.is_empty() {
                r.source_base_sha = legacy_resolved
                    .get(&format!("{}^1", r.parent_sha))
                    .cloned()
                    .unwrap_or_default();
                if r.original_shas.is_empty()
                    && let Some(original_sha) = legacy_revert_metric_original_sha(&r.parent_sha)
                {
                    r.original_shas.push(original_sha);
                }
            }
        }
    }

    resolved.retain(|r| !r.source_base_sha.is_empty());
    if resolved.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-read all source notes in one call.
    let note_shas: Vec<String> = {
        let mut v: Vec<String> = resolved
            .iter()
            .flat_map(|r| [r.source_base_sha.clone(), r.revert_commit.clone()])
            .collect();
        v.sort();
        v.dedup();
        v
    };
    let notes = notes_api::read_notes_batch(repo, &note_shas)?;

    // Build one batched diff-tree request covering, for each reverted commit:
    //  - (source_base, revert_commit): hunks to shift the source note forward,
    //  - (parent, revert_commit): added lines re-introduced by the revert.
    // Track each pair's index so we can read its result back.
    let mut diff_pairs: Vec<(String, String)> = Vec::new();
    let mut shift_idx: Vec<Option<usize>> = Vec::new();
    let mut added_idx: Vec<usize> = Vec::new();
    for r in &resolved {
        // The direct-note path and the history-aware fallback both shift from
        // the source base into the revert commit. Keep all pairs in this one
        // batched diff invocation.
        let shift = {
            let idx = diff_pairs.len();
            diff_pairs.push((r.source_base_sha.clone(), r.revert_commit.clone()));
            Some(idx)
        };
        shift_idx.push(shift);
        let aidx = diff_pairs.len();
        diff_pairs.push((r.parent_sha.clone(), r.revert_commit.clone()));
        added_idx.push(aidx);
    }
    let diff_results = compute_diff_trees_batch(repo, &diff_pairs)?;

    // Per-commit reconstruction is now pure in-memory.
    let mut reconstructed_by_commit: HashMap<String, AuthorshipLog> = HashMap::new();
    let mut added_lines_by_commit: HashMap<String, HashMap<String, Vec<u32>>> = HashMap::new();
    let mut metric_commits: Vec<RewriteMetricCommit> = Vec::new();
    for (i, r) in resolved.iter().enumerate() {
        // Added lines re-introduced by the revert (new-side hunk ranges of the
        // parent->revert diff), keyed by file.
        let added_lines = added_lines_from_diff_result(&diff_results[added_idx[i]]);
        if added_lines.is_empty() {
            continue;
        }

        let log = shift_idx[i]
            .and_then(|shift| {
                notes
                    .get(&r.source_base_sha)
                    .and_then(|note| AuthorshipLog::deserialize_from_string(note).ok())
                    .map(|log| {
                        shift_and_clip_revert_log(
                            log,
                            &diff_results[shift],
                            &added_lines,
                            &r.revert_commit,
                        )
                    })
            })
            .unwrap_or_default();

        let commit_added = added_lines_by_commit
            .entry(r.revert_commit.clone())
            .or_default();
        for (path, lines) in &added_lines {
            let target = commit_added.entry(path.clone()).or_default();
            target.extend(lines.iter().copied());
            target.sort_unstable();
            target.dedup();
        }
        if !log.attestations.is_empty() {
            match reconstructed_by_commit.get_mut(&r.revert_commit) {
                Some(existing) => crate::authorship::rewrite::merge_authorship_logs(existing, &log),
                None => {
                    reconstructed_by_commit.insert(r.revert_commit.clone(), log);
                }
            }
        }
        if collect_metrics {
            metric_commits.push(
                RewriteMetricCommit::new(
                    r.revert_commit.clone(),
                    r.original_shas.clone(),
                    RewriteMetricOperation::Revert,
                )
                .with_parent_sha(r.parent_sha.clone())
                .with_parent_diff(diff_results[added_idx[i]].clone()),
            );
        }
    }

    // Direct source-base notes are intentionally the constant-time fast path.
    // If they do not cover every restored line, scan the ORIGINAL pre-revert
    // history once, batch-read all reachable notes, and batch-compute every
    // required note->destination shift. Processing notes newest-first and only
    // filling uncovered ranges reproduces blame precedence without spawning a
    // git process per reverted commit or per file.
    let needs_history_fallback = added_lines_by_commit.iter().any(|(commit, added)| {
        reconstructed_by_commit
            .get(commit)
            .is_none_or(|log| !uncovered_added_lines(added, log).is_empty())
    });
    if needs_history_fallback {
        let history_tips = resolved
            .iter()
            .map(|r| r.parent_sha.clone())
            .collect::<Vec<_>>();
        let relevant_paths = added_lines_by_commit
            .values()
            .flat_map(|added| added.keys().cloned())
            .collect::<HashSet<_>>();
        let history_commits = rev_list_for_revert_fallback(repo, &history_tips, &relevant_paths)?;
        let history_notes = notes_api::read_notes_batch(repo, &history_commits)?;
        let history_logs = history_commits
            .iter()
            .filter_map(|commit| {
                let note = history_notes.get(commit)?;
                let log = AuthorshipLog::deserialize_from_string(note).ok()?;
                log.attestations
                    .iter()
                    .any(|file| relevant_paths.contains(&file.file_path))
                    .then(|| (commit.clone(), log))
            })
            .collect::<Vec<_>>();
        let mut fallback_pairs = Vec::new();
        let mut fallback_pair_index = HashMap::new();
        for (destination, added) in &added_lines_by_commit {
            for (source, log) in &history_logs {
                if source == destination
                    || !log
                        .attestations
                        .iter()
                        .any(|file| added.contains_key(&file.file_path))
                {
                    continue;
                }
                fallback_pair_index
                    .insert((destination.clone(), source.clone()), fallback_pairs.len());
                fallback_pairs.push((source.clone(), destination.clone()));
            }
        }
        let fallback_diffs = compute_diff_trees_batch(repo, &fallback_pairs)?;

        for (destination, added) in &added_lines_by_commit {
            let target = reconstructed_by_commit
                .entry(destination.clone())
                .or_default();
            for (source, log) in &history_logs {
                let missing = uncovered_added_lines(added, target);
                if missing.is_empty() {
                    break;
                }
                let Some(index) = fallback_pair_index
                    .get(&(destination.clone(), source.clone()))
                    .copied()
                else {
                    continue;
                };
                let shifted = shift_and_clip_revert_log(
                    log.clone(),
                    &fallback_diffs[index],
                    &missing,
                    destination,
                );
                crate::authorship::rewrite::merge_authorship_logs(target, &shifted);
            }
        }
    }

    let mut writes = Vec::new();
    for (commit, source_log) in reconstructed_by_commit {
        let final_log = notes
            .get(&commit)
            .and_then(|note| AuthorshipLog::deserialize_from_string(note).ok())
            .map(|resolution_log| {
                crate::authorship::conflict_resolution::merge_conflict_resolution_authorship(
                    Some(source_log.clone()),
                    resolution_log,
                    &commit,
                )
            })
            .unwrap_or(source_log);
        let note = final_log.serialize_to_string().map_err(|error| {
            GitAiError::Generic(format!(
                "failed to serialize revert authorship log: {error}"
            ))
        })?;
        for metric in metric_commits
            .iter_mut()
            .filter(|metric| metric.new_sha == commit)
        {
            metric.authorship_note = Some(note.clone());
        }
        writes.push((commit, note));
    }

    if !writes.is_empty() {
        notes_api::write_notes_batch(repo, &writes)?;
    }
    Ok(metric_commits)
}

fn rev_list_for_revert_fallback(
    repo: &Repository,
    history_tips: &[String],
    relevant_paths: &HashSet<String>,
) -> Result<Vec<String>, GitAiError> {
    const MAX_REVERT_FALLBACK_COMMITS: usize = 256;

    if history_tips.is_empty() || relevant_paths.is_empty() {
        return Ok(Vec::new());
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--topo-order".to_string(),
        "--date-order".to_string(),
        format!("--max-count={MAX_REVERT_FALLBACK_COMMITS}"),
    ]);
    args.extend(history_tips.iter().filter(|tip| !tip.is_empty()).cloned());
    args.push("--".to_string());
    args.extend(relevant_paths.iter().cloned());
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn uncovered_added_lines(
    added_lines: &HashMap<String, Vec<u32>>,
    log: &AuthorshipLog,
) -> HashMap<String, Vec<u32>> {
    let mut covered: HashMap<&str, HashSet<u32>> = HashMap::new();
    for file in &log.attestations {
        let lines = covered.entry(&file.file_path).or_default();
        for entry in &file.entries {
            for range in &entry.line_ranges {
                lines.extend(range.expand());
            }
        }
    }
    added_lines
        .iter()
        .filter_map(|(path, lines)| {
            let covered = covered.get(path.as_str());
            let missing = lines
                .iter()
                .copied()
                .filter(|line| covered.is_none_or(|covered| !covered.contains(line)))
                .collect::<Vec<_>>();
            (!missing.is_empty()).then(|| (path.clone(), missing))
        })
        .collect()
}

fn shift_and_clip_revert_log(
    mut log: AuthorshipLog,
    shift_result: &crate::authorship::rewrite::DiffTreeResult,
    added_lines: &HashMap<String, Vec<u32>>,
    revert_commit: &str,
) -> AuthorshipLog {
    for (old_path, new_path) in &shift_result.renames {
        for attestation in &mut log.attestations {
            if attestation.file_path == *old_path {
                attestation.file_path = new_path.clone();
            }
        }
    }
    if !shift_result.hunks_by_file.is_empty() {
        log.attestations = log
            .attestations
            .iter()
            .filter_map(
                |file| match shift_result.hunks_by_file.get(&file.file_path) {
                    Some(hunks) => apply_hunk_shifts_to_file_attestation(file, hunks),
                    None => Some(file.clone()),
                },
            )
            .collect();
    }
    log.metadata.base_commit_sha = revert_commit.to_string();
    log.attestations = log
        .attestations
        .iter()
        .filter_map(|file| clip_file_attestation_to_lines(file, added_lines))
        .collect();
    log
}

fn legacy_revert_metric_original_sha(parent_sha: &str) -> Option<String> {
    if parent_sha.is_empty() {
        None
    } else {
        Some(parent_sha.to_string())
    }
}

/// Extract added line numbers per file from a diff-tree result, equivalent to
/// the new-side coverage `diff_added_lines` would report for the same pair.
fn added_lines_from_diff_result(
    result: &crate::authorship::rewrite::DiffTreeResult,
) -> HashMap<String, Vec<u32>> {
    let mut added: HashMap<String, Vec<u32>> = HashMap::new();
    for (file, hunks) in &result.hunks_by_file {
        let mut lines = Vec::new();
        for hunk in hunks {
            for line in hunk.new_start..hunk.new_start + hunk.new_count {
                if line > 0 {
                    lines.push(line);
                }
            }
        }
        if !lines.is_empty() {
            added.insert(file.clone(), lines);
        }
    }
    added
}

/// Batch `git rev-parse --verify <revspec>...` for many revspecs in one spawn.
/// Returns a map from input revspec → resolved OID (only successful entries).
fn batch_rev_parse_verify(
    repo: &Repository,
    revspecs: &[String],
) -> Result<HashMap<String, String>, GitAiError> {
    let mut out = HashMap::new();
    if revspecs.is_empty() {
        return Ok(out);
    }
    let mut args = repo.global_args_for_exec();
    args.push("rev-parse".to_string());
    args.push("--verify".to_string());
    for spec in revspecs {
        args.push(spec.clone());
    }
    // rev-parse prints one resolved OID per input line, in order. If every spec
    // resolves, this single spawn is all we need.
    if let Ok(output) = exec_git(&args) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        let oids: Vec<&str> = stdout.lines().collect();
        if oids.len() == revspecs.len() {
            for (spec, oid) in revspecs.iter().zip(oids) {
                out.insert(spec.clone(), oid.trim().to_string());
            }
            return Ok(out);
        }
    }
    // A bad revspec makes `rev-parse --verify` fail for the whole batch. Resolve
    // failure-tolerantly with a SINGLE `cat-file --batch-check` spawn (one line
    // of output per input, "<oid> <type> <size>" or "<spec> missing"), so we
    // never fall back to per-spec spawning.
    let mut check_args = repo.global_args_for_exec();
    check_args.push("cat-file".to_string());
    check_args.push("--batch-check".to_string());
    let stdin_data = format!("{}\n", revspecs.join("\n"));
    if let Ok(output) = exec_git_stdin(&check_args, stdin_data.as_bytes()) {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for (spec, line) in revspecs.iter().zip(stdout.lines()) {
            let mut parts = line.split_whitespace();
            if let (Some(oid), Some(kind)) = (parts.next(), parts.next())
                && kind != "missing"
                && crate::git::repo_state::is_valid_git_oid(oid)
            {
                out.insert(spec.clone(), oid.to_string());
            }
        }
    }
    Ok(out)
}

fn clip_file_attestation_to_lines(
    file: &FileAttestation,
    added_lines: &HashMap<String, Vec<u32>>,
) -> Option<FileAttestation> {
    let target_lines = added_lines.get(&file.file_path)?;
    let target_lines = target_lines.iter().copied().collect::<HashSet<_>>();
    let mut entries = Vec::new();

    for entry in &file.entries {
        let mut lines = entry
            .line_ranges
            .iter()
            .flat_map(LineRange::expand)
            .filter(|line| target_lines.contains(line))
            .collect::<Vec<_>>();
        if lines.is_empty() {
            continue;
        }
        lines.sort_unstable();
        lines.dedup();
        entries.push(AttestationEntry::new(
            entry.hash.clone(),
            LineRange::compress_lines(&lines),
        ));
    }

    (!entries.is_empty()).then(|| FileAttestation {
        file_path: file.file_path.clone(),
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_revert_metric_original_sha_uses_reverted_commit() {
        assert_eq!(
            legacy_revert_metric_original_sha("reverted-commit"),
            Some("reverted-commit".to_string())
        );
        assert_eq!(legacy_revert_metric_original_sha(""), None);
    }
}
