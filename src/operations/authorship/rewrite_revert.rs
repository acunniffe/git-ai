use std::collections::{HashMap, HashSet};

use crate::clients::git_cli::{exec_git, exec_git_stdin};
use crate::error::GitAiError;
use crate::model::authorship_log::LineRange;
use crate::model::authorship_log_serialization::{
    AttestationEntry, AuthorshipLog, FileAttestation,
};
use crate::model::hunk_shift::apply_hunk_shifts_to_file_attestation;
use crate::operations::authorship::rewrite::compute_diff_trees_batch;
use crate::operations::authorship::rewrite::{RewriteMetricCommit, RewriteMetricOperation};
use crate::operations::git::notes_api;
use crate::operations::git::repository::Repository;

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
pub(crate) fn handle_revert_commits_with_metrics(
    repo: &Repository,
    specs: &[RevertSpec],
) -> Result<Vec<RewriteMetricCommit>, GitAiError> {
    if specs.is_empty() {
        return Ok(Vec::new());
    }
    let collect_metrics = crate::operations::authorship::rewrite::rewrite_metrics_enabled();

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
    let source_base_shas: Vec<String> = {
        let mut v: Vec<String> = resolved.iter().map(|r| r.source_base_sha.clone()).collect();
        v.sort();
        v.dedup();
        v
    };
    let notes = notes_api::read_notes_batch(repo, &source_base_shas)?;

    // Build one batched diff-tree request covering, for each reverted commit:
    //  - (source_base, revert_commit): hunks to shift the source note forward,
    //  - (parent, revert_commit): added lines re-introduced by the revert.
    // Track each pair's index so we can read its result back.
    let mut diff_pairs: Vec<(String, String)> = Vec::new();
    let mut shift_idx: Vec<Option<usize>> = Vec::new();
    let mut added_idx: Vec<usize> = Vec::new();
    for r in &resolved {
        // Only need the shift pair if the source note exists.
        let shift = if notes.contains_key(&r.source_base_sha) {
            let idx = diff_pairs.len();
            diff_pairs.push((r.source_base_sha.clone(), r.revert_commit.clone()));
            Some(idx)
        } else {
            None
        };
        shift_idx.push(shift);
        let aidx = diff_pairs.len();
        diff_pairs.push((r.parent_sha.clone(), r.revert_commit.clone()));
        added_idx.push(aidx);
    }
    let diff_results = compute_diff_trees_batch(repo, &diff_pairs)?;

    // Per-commit reconstruction is now pure in-memory.
    let mut writes: Vec<(String, String)> = Vec::new();
    let mut metric_commits: Vec<RewriteMetricCommit> = Vec::new();
    for (i, r) in resolved.iter().enumerate() {
        let Some(shift) = shift_idx[i] else {
            continue;
        };
        let Some(source_note) = notes.get(&r.source_base_sha) else {
            continue;
        };
        let Ok(mut log) = AuthorshipLog::deserialize_from_string(source_note) else {
            continue;
        };

        // Added lines re-introduced by the revert (new-side hunk ranges of the
        // parent->revert diff), keyed by file.
        let added_lines = added_lines_from_diff_result(&diff_results[added_idx[i]]);
        if added_lines.is_empty() {
            continue;
        }

        let shift_result = &diff_results[shift];
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
                .filter_map(|fa| match shift_result.hunks_by_file.get(&fa.file_path) {
                    Some(hunks) => apply_hunk_shifts_to_file_attestation(fa, hunks),
                    None => Some(fa.clone()),
                })
                .collect();
        }

        log.metadata.base_commit_sha = r.revert_commit.clone();
        log.attestations = log
            .attestations
            .iter()
            .filter_map(|file| clip_file_attestation_to_lines(file, &added_lines))
            .collect();
        if log.attestations.is_empty() {
            continue;
        }

        let Ok(note_str) = log.serialize_to_string() else {
            continue;
        };
        writes.push((r.revert_commit.clone(), note_str.clone()));
        if collect_metrics {
            metric_commits.push(
                RewriteMetricCommit::new(
                    r.revert_commit.clone(),
                    r.original_shas.clone(),
                    RewriteMetricOperation::Revert,
                )
                .with_parent_sha(r.parent_sha.clone())
                .with_authorship_note(note_str)
                .with_parent_diff(diff_results[added_idx[i]].clone()),
            );
        }
    }

    if !writes.is_empty() {
        notes_api::write_notes_batch(repo, &writes)?;
    }
    Ok(metric_commits)
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
    result: &crate::operations::authorship::rewrite::DiffTreeResult,
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
                && crate::operations::git::oid::is_full_oid(oid)
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
