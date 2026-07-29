//! Reconciliation of rebases that completed while no daemon was observing.
//!
//! The live pipeline shifts authorship notes onto rebased commits only when
//! the daemon observes the rebase (trace2 → `RefCursor` → non-fast-forward
//! rewrite detection). If a rebase runs while the daemon is down — or while
//! its trace ingestion is broken — the note stays on the pre-rebase commit
//! forever: cursor offsets live only in memory, and cold seeding deliberately
//! treats reflog history from before the first observed command as prior
//! untraced history. This module closes that gap: recently completed rebase
//! spans are recovered from the HEAD reflog and any span that stranded notes
//! is replayed through the normal non-fast-forward shift path (which
//! merges/skips targets that already have notes, so replaying an
//! already-handled span is a no-op).

use crate::daemon::domain::RefChange;
use crate::error::GitAiError;
use crate::git::notes_api;
use crate::git::repository::{Repository, exec_git, exec_git_stdin};
use std::collections::HashSet;

/// Upper bound on reflog entries scanned (from the tail) per worktree.
pub(crate) const MAX_REFLOG_ENTRIES: usize = 512;
/// Only spans that finished within this window are reconciled.
pub(crate) const MAX_SPAN_AGE_SECS: i64 = 14 * 24 * 60 * 60;
/// Hard cap on spans considered per worktree (newest kept), so the total git
/// work is bounded by a constant regardless of how rebase-heavy the reflog is.
pub(crate) const MAX_RECONCILE_SPANS: usize = 8;
/// Upper bound on commits listed per span side when probing for stranded notes.
const MAX_SPAN_COMMITS: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CompletedRebaseSpan {
    /// HEAD before `rebase (start)` — the pre-rebase tip whose notes may be stranded.
    pub(crate) old_tip: String,
    /// HEAD at `rebase (finish)` — the rewritten tip that may be missing notes.
    pub(crate) new_tip: String,
    /// The commit checked out by `rebase (start)` (the rebase target / `--onto`).
    pub(crate) onto: String,
    /// Reflog timestamp (epoch seconds) of the finish entry.
    pub(crate) finished_at_secs: i64,
    /// Distinct commits the span's own reflog rows record as created by the
    /// rewrite (`new` of every row after the start, e.g. picks and the
    /// finish). Free per-span target data — no git spawn needed — used to
    /// screen out spans whose rewrites are all already noted. May include
    /// intermediates later superseded within the same rebase; that only
    /// widens candidacy, and the precise probe rejects false candidates.
    pub(crate) rewritten_commits: Vec<String>,
}

#[derive(Debug)]
struct ReflogEntry {
    old: String,
    new: String,
    timestamp_secs: i64,
    message: String,
}

fn parse_reflog_entry(line: &str) -> Option<ReflogEntry> {
    let (left, message) = line.split_once('\t')?;
    let fields: Vec<&str> = left.split_whitespace().collect();
    // "<old> <new> <ident...> <epoch-secs> <tz>"
    if fields.len() < 4 {
        return None;
    }
    let timestamp_secs = fields[fields.len() - 2].parse::<i64>().ok()?;
    Some(ReflogEntry {
        old: fields[0].to_string(),
        new: fields[1].to_string(),
        timestamp_secs,
        message: message.to_string(),
    })
}

/// Parse completed `rebase (start) … rebase (finish)` spans out of a HEAD
/// reflog, oldest first. Only the last [`MAX_REFLOG_ENTRIES`] entries are
/// examined. Spans without a finish row are ignored: a conflicted rebase is
/// still owned by the live path once it continues, and an abort restores the
/// pre-rebase tip so there is nothing to shift.
pub(crate) fn completed_rebase_spans(head_reflog: &str) -> Vec<CompletedRebaseSpan> {
    let lines: Vec<&str> = head_reflog.lines().collect();
    let tail_start = lines.len().saturating_sub(MAX_REFLOG_ENTRIES);

    let mut spans = Vec::new();
    let mut open_start: Option<ReflogEntry> = None;
    let mut open_rewritten: Vec<String> = Vec::new();
    for line in &lines[tail_start..] {
        let Some(entry) = parse_reflog_entry(line) else {
            continue;
        };
        // Covers both "rebase (...)" and legacy "rebase -i (...)" messages.
        let is_rebase = entry.message.starts_with("rebase");
        if is_rebase && entry.message.contains("(start)") {
            open_start = Some(entry);
            open_rewritten.clear();
        } else if is_rebase && entry.message.contains("(finish)") {
            if let Some(start) = open_start.take() {
                let mut rewritten_commits = std::mem::take(&mut open_rewritten);
                rewritten_commits.push(entry.new.clone());
                rewritten_commits.sort();
                rewritten_commits.dedup();
                rewritten_commits.retain(|sha| *sha != start.new);
                spans.push(CompletedRebaseSpan {
                    old_tip: start.old,
                    onto: start.new,
                    new_tip: entry.new,
                    finished_at_secs: entry.timestamp_secs,
                    rewritten_commits,
                });
            }
        } else if !is_rebase || entry.message.contains("(abort)") {
            // Any non-rebase HEAD move (or an abort) while a span is open
            // means that rebase never finished as a rewrite of this branch.
            open_start = None;
            open_rewritten.clear();
        } else if open_start.is_some() {
            open_rewritten.push(entry.new.clone());
        }
    }
    spans
}

/// True when the span's tips line up with a real ref movement observed for
/// the current command — i.e. the span was produced by this command and is
/// owned by the live non-fast-forward path, not left over from an earlier
/// unobserved rebase. A no-op rebase ("current branch is up to date") writes
/// no reflog span and observes no movement, and a fast-forward `pull
/// --rebase` moves HEAD between tips unrelated to a stranded span, so
/// neither matches.
pub(crate) fn span_matches_command_ref_changes(
    span: &CompletedRebaseSpan,
    ref_changes: &[RefChange],
) -> bool {
    ref_changes
        .iter()
        .filter(|change| change.old != change.new)
        .any(|change| change.old == span.old_tip || change.new == span.new_tip)
}

/// True when the span moved notes-bearing work onto commits that are missing
/// notes: at least one source commit (reachable from `old_tip` only) has an
/// authorship note while at least one target commit (reachable from `new_tip`
/// only) has none. Keeps reconciliation from re-fetching/re-shifting spans
/// that were already handled or never carried AI work.
pub(crate) fn span_has_stranded_notes(
    repo: &Repository,
    span: &CompletedRebaseSpan,
) -> Result<bool, GitAiError> {
    let sources = rev_list_capped(repo, &span.old_tip, &span.new_tip)?;
    if sources.is_empty() {
        return Ok(false);
    }
    if notes_api::commits_with_notes(repo, &sources)?.is_empty() {
        return Ok(false);
    }
    let targets = rev_list_capped(repo, &span.new_tip, &span.old_tip)?;
    if targets.is_empty() {
        return Ok(false);
    }
    let noted_targets = notes_api::commits_with_notes(repo, &targets)?;
    Ok(noted_targets.len() < targets.len())
}

/// Batched existence probe: a single `git cat-file --batch-check` spawn
/// validates every candidate tip at once (never one spawn per object).
/// Returns the subset of `shas` that resolve to commits.
pub(crate) fn existing_commits(
    repo: &Repository,
    shas: &[String],
) -> Result<HashSet<String>, GitAiError> {
    if shas.is_empty() {
        return Ok(HashSet::new());
    }
    let mut args = repo.global_args_for_exec();
    args.extend(["cat-file".to_string(), "--batch-check".to_string()]);
    let stdin = shas.join("\n");
    let output = exec_git_stdin(&args, stdin.as_bytes())?;
    let mut existing = HashSet::new();
    for line in String::from_utf8_lossy(&output.stdout).lines() {
        let mut tokens = line.split_whitespace();
        if let (Some(oid), Some(kind)) = (tokens.next(), tokens.next())
            && kind == "commit"
        {
            existing.insert(oid.to_string());
        }
    }
    Ok(existing)
}

/// Cheap screen using only reflog-derived data plus one batched note lookup:
/// a span whose recorded rewritten commits all already carry notes was either
/// handled live or reconciled before — it cannot be stranded. Spans with no
/// recorded rewrites stay candidates so the precise probe can decide.
pub(crate) fn may_have_stranded_notes(
    span: &CompletedRebaseSpan,
    noted_rewritten: &HashSet<String>,
) -> bool {
    span.rewritten_commits.is_empty()
        || span
            .rewritten_commits
            .iter()
            .any(|sha| !noted_rewritten.contains(sha))
}

fn rev_list_capped(
    repo: &Repository,
    include: &str,
    exclude: &str,
) -> Result<Vec<String>, GitAiError> {
    let mut args = repo.global_args_for_exec();
    args.extend([
        "rev-list".to_string(),
        format!("--max-count={MAX_SPAN_COMMITS}"),
        include.to_string(),
        format!("^{exclude}"),
    ]);
    let output = exec_git(&args)?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| line.trim().to_string())
        .filter(|line| !line.is_empty())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
    const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
    const D: &str = "dddddddddddddddddddddddddddddddddddddddd";
    const E: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

    fn entry(old: &str, new: &str, ts: i64, message: &str) -> String {
        format!("{old} {new} Test User <test@example.com> {ts} +0000\t{message}\n")
    }

    #[test]
    fn extracts_completed_span() {
        let reflog = [
            entry(A, B, 100, "commit: base"),
            entry(B, C, 200, "rebase (start): checkout main"),
            entry(C, D, 201, "rebase (pick): feature work"),
            entry(D, D, 202, "rebase (finish): returning to refs/heads/feature"),
        ]
        .concat();
        let spans = completed_rebase_spans(&reflog);
        assert_eq!(
            spans,
            vec![CompletedRebaseSpan {
                old_tip: B.to_string(),
                new_tip: D.to_string(),
                onto: C.to_string(),
                finished_at_secs: 202,
                rewritten_commits: vec![D.to_string()],
            }]
        );
    }

    #[test]
    fn ignores_unfinished_and_aborted_spans() {
        let unfinished = [
            entry(B, C, 200, "rebase (start): checkout main"),
            entry(C, D, 201, "rebase (pick): feature work"),
        ]
        .concat();
        assert!(completed_rebase_spans(&unfinished).is_empty());

        let aborted = [
            entry(B, C, 200, "rebase (start): checkout main"),
            entry(C, B, 201, "rebase (abort): returning to refs/heads/feature"),
            entry(B, E, 300, "commit: after abort"),
        ]
        .concat();
        assert!(completed_rebase_spans(&aborted).is_empty());
    }

    #[test]
    fn non_rebase_entry_closes_open_span() {
        // A checkout in the middle of a conflicted rebase abandons the span;
        // a later stray finish row must not pair with the stale start.
        let reflog = [
            entry(B, C, 200, "rebase (start): checkout main"),
            entry(C, A, 210, "checkout: moving from feature to main"),
            entry(A, E, 300, "rebase (finish): returning to refs/heads/other"),
        ]
        .concat();
        assert!(completed_rebase_spans(&reflog).is_empty());
    }

    #[test]
    fn extracts_multiple_spans_oldest_first() {
        let reflog = [
            entry(A, B, 100, "rebase (start): checkout main"),
            entry(B, C, 101, "rebase (finish): returning to refs/heads/x"),
            entry(C, C, 150, "commit (amend): tweak"),
            entry(C, D, 200, "rebase -i (start): checkout main"),
            entry(D, E, 201, "rebase -i (finish): returning to refs/heads/x"),
        ]
        .concat();
        let spans = completed_rebase_spans(&reflog);
        assert_eq!(spans.len(), 2);
        assert_eq!(spans[0].new_tip, C);
        assert_eq!(spans[0].finished_at_secs, 101);
        assert_eq!(spans[1].old_tip, C);
        assert_eq!(spans[1].new_tip, E);
    }

    fn ref_change(old: &str, new: &str) -> RefChange {
        RefChange {
            reference: "HEAD".to_string(),
            old: old.to_string(),
            new: new.to_string(),
        }
    }

    fn span(old_tip: &str, new_tip: &str) -> CompletedRebaseSpan {
        CompletedRebaseSpan {
            old_tip: old_tip.to_string(),
            new_tip: new_tip.to_string(),
            onto: C.to_string(),
            finished_at_secs: 100,
            rewritten_commits: vec![new_tip.to_string()],
        }
    }

    #[test]
    fn screen_drops_spans_whose_rewrites_are_all_noted() {
        let noted: HashSet<String> = [B.to_string()].into();
        // All rewritten commits noted -> already handled, not a candidate.
        assert!(!may_have_stranded_notes(&span(A, B), &noted));
        // An unnoted rewritten commit keeps the span a candidate.
        assert!(may_have_stranded_notes(&span(A, D), &noted));
        // No recorded rewrites: stay a candidate for the precise probe.
        let mut bare = span(A, B);
        bare.rewritten_commits.clear();
        assert!(may_have_stranded_notes(&bare, &noted));
    }

    #[test]
    fn span_matches_command_that_produced_it() {
        // A live rebase's enriched ref changes start at the span's old tip
        // and end at its new tip.
        let changes = [ref_change(A, C), ref_change(C, B)];
        assert!(span_matches_command_ref_changes(&span(A, B), &changes));
    }

    #[test]
    fn stranded_span_does_not_match_noop_or_fast_forward_command() {
        let stranded = span(A, B);
        // No-op rebase ("current branch is up to date"): no ref movement.
        assert!(!span_matches_command_ref_changes(&stranded, &[]));
        // Degenerate change (old == new) must not count as the span's owner.
        assert!(!span_matches_command_ref_changes(
            &stranded,
            &[ref_change(B, B)]
        ));
        // Fast-forward pull --rebase moves HEAD onward from the stranded
        // span's new tip; neither endpoint pairs with the span.
        assert!(!span_matches_command_ref_changes(
            &stranded,
            &[ref_change(B, D)]
        ));
    }

    #[test]
    fn malformed_lines_are_skipped() {
        let reflog = format!(
            "not a reflog line\n{}{}",
            entry(B, C, 200, "rebase (start): checkout main"),
            entry(C, D, 201, "rebase (finish): returning to refs/heads/feature"),
        );
        assert_eq!(completed_rebase_spans(&reflog).len(), 1);
    }
}
