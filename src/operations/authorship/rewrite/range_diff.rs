use std::collections::HashMap;

use crate::clients::git_cli::{exec_git, exec_git_allow_nonzero};
use crate::error::GitAiError;
use crate::operations::git::notes_api;
use crate::operations::git::oid::{is_full_oid, is_zero_oid};
use crate::operations::git::repository::Repository;

/// Derive old→new commit mappings by running `git range-diff` between the
/// old and new branch tips, then mapping merge commits structurally.
pub(super) fn derive_mappings_from_range_diff(
    repo: &Repository,
    old_tip: &str,
    new_tip: &str,
    onto_hint: Option<&str>,
) -> Result<Vec<(String, String)>, GitAiError> {
    let Some(base) = find_merge_base(repo, old_tip, new_tip) else {
        return Ok(Vec::new());
    };

    // Rewind: branch moved backward
    if base == new_tip {
        crate::operations::authorship::rewrite_reset::reconstruct_working_log_after_backward_reset(
            repo, old_tip, new_tip,
        )?;
        return Ok(Vec::new());
    }

    // Fast-forward: no rewrite happened
    if base == old_tip {
        return Ok(Vec::new());
    }

    // Validate onto_hint: it must be an ancestor of new_tip and different from new_tip.
    // If the hint is invalid (e.g., from a checkout-then-rebase where first HEAD change
    // is the checkout, not the rebase), fall back to base.
    let onto = match onto_hint {
        Some(hint) if hint != new_tip && hint != old_tip && is_ancestor(repo, hint, new_tip) => {
            hint
        }
        _ => &base,
    };
    let range_diff_output = run_range_diff(repo, &base, old_tip, onto, new_tip)?;
    let mut mappings = parse_range_diff_output(&range_diff_output);

    let merge_mappings = derive_merge_commit_mappings(repo, &base, old_tip, new_tip, &mappings)?;
    mappings.extend(merge_mappings);

    Ok(mappings)
}

fn is_ancestor(repo: &Repository, ancestor: &str, descendant: &str) -> bool {
    repo.is_ancestor(ancestor, descendant).unwrap_or(false)
}

pub(super) fn find_merge_base(repo: &Repository, a: &str, b: &str) -> Option<String> {
    let mut args = repo.global_args_for_exec();
    args.extend(["merge-base".to_string(), a.to_string(), b.to_string()]);

    let output = exec_git_allow_nonzero(&args).ok()?;
    if !output.status.success() {
        return None;
    }
    let base = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if base.is_empty() { None } else { Some(base) }
}

pub(crate) fn list_commits_in_range(repo: &Repository, base: &str, tip: &str) -> Vec<String> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--reverse".to_string(),
        format!("{}..{}", base, tip),
    ]);
    exec_git_allow_nonzero(&args)
        .ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty())
                .collect()
        })
        .unwrap_or_default()
}

fn run_range_diff(
    repo: &Repository,
    old_base: &str,
    old_tip: &str,
    new_base: &str,
    new_tip: &str,
) -> Result<String, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "range-diff".to_string(),
        "--no-color".to_string(),
        "--no-abbrev".to_string(),
        "-s".to_string(),
        "--creation-factor=100".to_string(),
        format!("{}..{}", old_base, old_tip),
        format!("{}..{}", new_base, new_tip),
    ]);
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Maximum number of unmatched old-range commits buffered before the first
/// real range-diff match. Small leading groups can represent a valid squash,
/// but a larger group indicates divergent history such as a restack undo.
pub(super) const MAX_PENDING_DROPPED_COMMITS: usize = 64;

pub(super) fn parse_range_diff_output(output: &str) -> Vec<(String, String)> {
    let mut mappings = Vec::new();
    let mut pending_dropped: Vec<String> = Vec::new();
    let mut pending_overflowed = false;
    let mut previous_new_sha: Option<String> = None;

    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Find the first full-width Git OID.
        let Some((old_sha, rest)) = find_next_sha(trimmed) else {
            continue;
        };

        // Skip whitespace, read status character
        let rest = rest.trim_start();
        let Some(status_char) = rest.chars().next() else {
            continue;
        };

        match status_char {
            '<' => {
                // Dropped commit (squashed into a later commit)
                if !is_zero_oid(&old_sha) {
                    if let Some(new_sha) = previous_new_sha.as_ref() {
                        mappings.push((old_sha, new_sha.clone()));
                    } else if !pending_overflowed {
                        pending_dropped.push(old_sha);
                        if pending_dropped.len() > MAX_PENDING_DROPPED_COMMITS {
                            pending_dropped.clear();
                            pending_dropped.shrink_to_fit();
                            pending_overflowed = true;
                        }
                    }
                }
            }
            '=' | '!' => {
                // Matched pair
                let after_status = &rest[status_char.len_utf8()..];
                let Some((new_sha, _)) = find_next_sha(after_status) else {
                    continue;
                };
                if is_zero_oid(&old_sha) || is_zero_oid(&new_sha) {
                    continue;
                }
                // Map any preceding dropped commits to this new commit (squash)
                for dropped in pending_dropped.drain(..) {
                    mappings.push((dropped, new_sha.clone()));
                }
                previous_new_sha = Some(new_sha.clone());
                mappings.push((old_sha, new_sha));
            }
            _ => {
                // '>' (new commit) or other — skip
                continue;
            }
        }
    }

    mappings
}

/// Find the first maximal ASCII-hex run in `s` whose length is a valid git OID
/// length (40 for SHA-1, 64 for SHA-256) and return it with the remainder of
/// the string after the run.
///
/// Scans over bytes rather than chars so a multibyte commit subject (e.g. a
/// range-diff `-s` line like `Café …`) never makes a window boundary land
/// inside a char and panic. Only a matched, all-ASCII window is converted to a
/// `String`. Taking the maximal run (delimited by non-hex on both sides) means
/// a 64-char SHA-256 OID is returned in full instead of truncated to 40.
fn find_next_sha(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_hexdigit() {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < bytes.len() && bytes[end].is_ascii_hexdigit() {
            end += 1;
        }
        let candidate = &s[start..end];
        if is_full_oid(candidate) {
            // The run is all ASCII hex, so slicing here is always char-safe.
            return Some((candidate.to_string(), &s[end..]));
        }
        // Not an OID-length run; skip past it entirely and keep scanning.
        i = end;
    }
    None
}

// DEFERRED (code-review #15): old->new merge commits are paired greedily by
// first parent-set match (the inner loop `break`s on the first new_merge whose
// parents all map). When two sibling merges in the same range share an
// identical parent mapping, the first-match pairing can attach old_merge A's
// note to new_merge B and vice versa. Harmless in the common single-merge case;
// a precise fix would disambiguate ties (e.g. by tree identity or commit order)
// instead of taking the first structural match.
fn derive_merge_commit_mappings(
    repo: &Repository,
    base: &str,
    old_tip: &str,
    new_tip: &str,
    existing_mappings: &[(String, String)],
) -> Result<Vec<(String, String)>, GitAiError> {
    let old_merges = list_merge_commits(repo, base, old_tip)?;
    let new_merges = list_merge_commits(repo, base, new_tip)?;

    if old_merges.is_empty() || new_merges.is_empty() {
        return Ok(Vec::new());
    }

    // Batch-check which old merges have notes
    let commits_with_notes = notes_api::commits_with_notes(repo, &old_merges)?;
    let merge_parent_map = get_commit_parents_batch(
        repo,
        &old_merges
            .iter()
            .chain(new_merges.iter())
            .cloned()
            .collect::<Vec<_>>(),
    );

    let mut merge_mappings: Vec<(String, String)> = Vec::new();

    for old_merge in &old_merges {
        if !commits_with_notes.contains(old_merge) {
            continue;
        }

        let old_parents = merge_parent_map.get(old_merge).cloned().unwrap_or_default();
        if old_parents.is_empty() {
            continue;
        }

        for new_merge in &new_merges {
            if merge_mappings.iter().any(|(_, n)| n == new_merge) {
                continue;
            }

            let new_parents = merge_parent_map.get(new_merge).cloned().unwrap_or_default();
            if new_parents.len() != old_parents.len() {
                continue;
            }

            let all_match = old_parents.iter().zip(new_parents.iter()).all(|(op, np)| {
                if existing_mappings.iter().any(|(o, n)| o == op && n == np) {
                    return true;
                }
                if merge_mappings.iter().any(|(o, n)| o == op && n == np) {
                    return true;
                }
                op == np
            });

            if all_match {
                merge_mappings.push((old_merge.clone(), new_merge.clone()));
                break;
            }
        }
    }

    Ok(merge_mappings)
}

fn list_merge_commits(repo: &Repository, base: &str, tip: &str) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        "--merges".to_string(),
        "--topo-order".to_string(),
        "--reverse".to_string(),
        format!("{}..{}", base, tip),
    ]);

    let output = exec_git_allow_nonzero(&args)?;
    if !output.status.success() {
        return Ok(Vec::new());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .lines()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect())
}

fn get_commit_parents_batch(repo: &Repository, shas: &[String]) -> HashMap<String, Vec<String>> {
    if shas.is_empty() {
        return HashMap::new();
    }
    let mut args = repo.global_args_for_exec();
    args.extend([
        "show".to_string(),
        "-s".to_string(),
        "--format=%H %P".to_string(),
        "--no-walk".to_string(),
    ]);
    args.extend(shas.iter().cloned());

    let Ok(output) = exec_git_allow_nonzero(&args) else {
        return HashMap::new();
    };
    if !output.status.success() {
        return HashMap::new();
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout
        .lines()
        .filter_map(|line| {
            let mut parts = line.split_whitespace();
            let sha = parts.next()?.to_string();
            let parents = parts.map(ToOwned::to_owned).collect::<Vec<_>>();
            Some((sha, parents))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_find_next_sha_rejects_non_oid_length_runs() {
        // A hex run that is neither 40 nor 64 chars is not an OID and must be
        // skipped (e.g. a short abbreviated hash or an index blob fragment).
        assert!(find_next_sha("deadbeef not a full oid").is_none());
        // 39 and 41 chars (off-by-one around SHA-1) are rejected.
        assert!(find_next_sha(&"a".repeat(39)).is_none());
        let nearly = format!("{} x", "a".repeat(41));
        assert!(find_next_sha(&nearly).is_none());
    }

    #[test]
    fn test_parse_range_diff_output_matched_equal() {
        let output = " 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb Some commit subject\n";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(mappings[0].1, "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb");
    }

    #[test]
    fn test_parse_range_diff_output_matched_bang() {
        let output = " 2:  1111111111111111111111111111111111111111 ! 3:  2222222222222222222222222222222222222222 Modified commit\n";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, "1111111111111111111111111111111111111111");
        assert_eq!(mappings[0].1, "2222222222222222222222222222222222222222");
    }

    #[test]
    fn test_parse_range_diff_output_dropped_and_new() {
        let output = "\
 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa < -:  0000000000000000000000000000000000000000 Dropped commit
 -:  0000000000000000000000000000000000000000 > 1:  cccccccccccccccccccccccccccccccccccccccc New commit
";
        let mappings = parse_range_diff_output(output);
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_parse_range_diff_output_dropped_then_matched_maps_both_to_destination() {
        let output = "\
1:  1111111111111111111111111111111111111111 < -:  ---------------------------------------- Add Python joke
2:  2222222222222222222222222222222222222222 ! 1:  3333333333333333333333333333333333333333 Add Rust joke
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(
            mappings,
            vec![
                (
                    "1111111111111111111111111111111111111111".to_string(),
                    "3333333333333333333333333333333333333333".to_string()
                ),
                (
                    "2222222222222222222222222222222222222222".to_string(),
                    "3333333333333333333333333333333333333333".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_parse_range_diff_output_matched_then_dropped_maps_all_to_destination() {
        let output = "\
1:  1111111111111111111111111111111111111111 ! 1:  4444444444444444444444444444444444444444 AI commit 1
2:  2222222222222222222222222222222222222222 < -:  ---------------------------------------- AI commit 2
3:  3333333333333333333333333333333333333333 < -:  ---------------------------------------- AI commit 3
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(
            mappings,
            vec![
                (
                    "1111111111111111111111111111111111111111".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
                (
                    "2222222222222222222222222222222222222222".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
                (
                    "3333333333333333333333333333333333333333".to_string(),
                    "4444444444444444444444444444444444444444".to_string()
                ),
            ]
        );
    }

    #[test]
    fn test_parse_range_diff_output_null_shas_skipped() {
        let output = " 1:  0000000000000000000000000000000000000000 = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb Subject\n";
        let mappings = parse_range_diff_output(output);
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_parse_range_diff_output_multiple_lines() {
        let output = "\
 1:  aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa = 1:  bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb First commit
 2:  cccccccccccccccccccccccccccccccccccccccc ! 2:  dddddddddddddddddddddddddddddddddddddddd Second commit
 3:  eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee = 3:  ffffffffffffffffffffffffffffffffffffffff Third commit
";
        let mappings = parse_range_diff_output(output);
        assert_eq!(mappings.len(), 3);
        assert_eq!(
            mappings[0],
            (
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string(),
                "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()
            )
        );
        assert_eq!(
            mappings[1],
            (
                "cccccccccccccccccccccccccccccccccccccccc".to_string(),
                "dddddddddddddddddddddddddddddddddddddddd".to_string()
            )
        );
        assert_eq!(
            mappings[2],
            (
                "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".to_string(),
                "ffffffffffffffffffffffffffffffffffffffff".to_string()
            )
        );
    }

    #[test]
    fn test_parse_range_diff_output_empty() {
        let mappings = parse_range_diff_output("");
        assert!(mappings.is_empty());
    }

    #[test]
    fn test_find_next_sha_does_not_panic_on_multibyte_subject() {
        // Regression (#1): find_next_sha sliced `&s[i..i+40]` by byte index. A
        // commit subject with a multibyte char ('é' at bytes 3..5) makes a
        // byte-window boundary land inside the char and panics
        // ("byte index 4 is not a char boundary; inside 'é'"). It must scan
        // safely and still find the trailing SHA.
        let sha = "a".repeat(40);
        let input = format!("Café commit subject {}", sha);
        let (found, rest) = find_next_sha(&input).expect("should find the trailing SHA");
        assert_eq!(found, sha);
        assert_eq!(rest, "");
    }

    #[test]
    fn test_find_next_sha_returns_full_sha256_oid() {
        // Regression (#10): a 64-char SHA-256 OID must be returned in full, not
        // truncated to the first 40 chars.
        let sha256 = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        assert_eq!(sha256.len(), 64);
        let input = format!("{} trailing", sha256);
        let (found, rest) = find_next_sha(&input).expect("should find the 64-char OID");
        assert_eq!(found, sha256);
        assert_eq!(rest, " trailing");
    }

    #[test]
    fn test_parse_range_diff_output_sha256() {
        // Regression (#10): range-diff with 64-char OIDs must map the full OIDs,
        // not 40-char truncations.
        let old = "1111111111111111111111111111111111111111111111111111111111111111";
        let new = "2222222222222222222222222222222222222222222222222222222222222222";
        let output = format!(" 1:  {} = 1:  {} Some subject\n", old, new);
        let mappings = parse_range_diff_output(&output);
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].0, old);
        assert_eq!(mappings[0].1, new);
    }

    #[test]
    fn test_parse_range_diff_output_skips_sha256_zero_oid() {
        let zero = "0".repeat(64);
        let old = "1".repeat(64);
        let new = "2".repeat(64);
        let output =
            format!(" 1:  {zero} = 1:  {new} Zero old\n 2:  {old} = 2:  {zero} Zero new\n");

        assert!(parse_range_diff_output(&output).is_empty());
    }
}
