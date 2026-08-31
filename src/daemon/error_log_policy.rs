//! Log-level policy for daemon side-effect and trace-ingest errors.
//!
//! All git processing is trace2-driven and asynchronous, so two error classes
//! are noise rather than actionable failures:
//!
//! - A machine whose git environment is persistently broken fails every
//!   side-effect pass with the identical error. Without suppression this
//!   produces one error log per user git invocation, indefinitely.
//! - A repository can be deleted between trace ingestion and async
//!   processing (temp repos created by tooling). The resulting failures are
//!   inherent to the async design and expected.
//!
//! The first few occurrences of a genuine failure still log at error level;
//! repeated identical failures downgrade to debug with a rate-limited warn
//! summary, and expected repo-gone conditions always log at debug.

use crate::error::GitAiError;
use std::time::{Duration, Instant};

/// Consecutive identical errors logged at error level before suppression.
const REPEATED_ERROR_LOG_LIMIT: u64 = 3;
/// Minimum interval between warn summaries of suppressed repeated errors.
const REPEATED_ERROR_SUMMARY_INTERVAL: Duration = Duration::from_secs(30 * 60);
/// Idle time after which a family's tracker is garbage-collected. Long enough
/// that an active storm never loses its suppression state between events.
const TRACKER_IDLE_EVICTION: Duration =
    Duration::from_secs(2 * REPEATED_ERROR_SUMMARY_INTERVAL.as_secs());

/// How a side-effect error should be logged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SideEffectErrorLog {
    /// Genuine failure that has not repeated: log at error level.
    Error,
    /// Expected consequence of the repository vanishing before async
    /// processing: log at debug level.
    ExpectedRepoGoneDebug,
    /// Identical error already logged `REPEATED_ERROR_LOG_LIMIT` times in a
    /// row: log at debug level.
    RepeatedDebug,
    /// Repeated identical error whose summary interval elapsed: log a warn
    /// summary including the occurrences downgraded since the last summary.
    RepeatedSummaryWarn { suppressed_count: u64 },
}

/// Tracks consecutive identical errors for one repo family.
#[derive(Debug, Default)]
pub(crate) struct RepeatedErrorTracker {
    last_key: Option<String>,
    consecutive_identical: u64,
    suppressed_since_summary: u64,
    last_summary_at: Option<Instant>,
    last_event_at: Option<Instant>,
}

impl RepeatedErrorTracker {
    pub(crate) fn on_error(&mut self, error: &GitAiError, now: Instant) -> SideEffectErrorLog {
        let key = suppression_key(error);
        if self.last_key.as_deref() != Some(key.as_str()) {
            *self = Self {
                last_key: Some(key),
                ..Self::default()
            };
        }
        self.last_event_at = Some(now);
        self.consecutive_identical = self.consecutive_identical.saturating_add(1);
        if self.consecutive_identical <= REPEATED_ERROR_LOG_LIMIT {
            if self.consecutive_identical == REPEATED_ERROR_LOG_LIMIT {
                // Suppression starts now; the first summary is due one full
                // interval later.
                self.last_summary_at = Some(now);
            }
            return SideEffectErrorLog::Error;
        }
        if self
            .last_summary_at
            .is_none_or(|at| now.duration_since(at) >= REPEATED_ERROR_SUMMARY_INTERVAL)
        {
            let suppressed_count = self.suppressed_since_summary;
            self.suppressed_since_summary = 0;
            self.last_summary_at = Some(now);
            return SideEffectErrorLog::RepeatedSummaryWarn { suppressed_count };
        }
        self.suppressed_since_summary = self.suppressed_since_summary.saturating_add(1);
        SideEffectErrorLog::RepeatedDebug
    }

    /// True once the tracker has been idle long enough to garbage-collect.
    pub(crate) fn is_stale(&self, now: Instant) -> bool {
        self.last_event_at
            .is_none_or(|at| now.duration_since(at) >= TRACKER_IDLE_EVICTION)
    }
}

/// Key under which errors are considered "identical" for suppression.
///
/// Git CLI failures are keyed on exit code and stderr only: argv embeds
/// per-invocation values (commit OIDs, `-C <path>`), so a persistently broken
/// environment would otherwise produce a distinct signature per command and
/// never be suppressed. Volatile hex runs (OIDs, pids, timestamps) inside the
/// text are normalized away for the same reason.
fn suppression_key(error: &GitAiError) -> String {
    match error {
        GitAiError::GitCliError { code, stderr, .. } => {
            format!("git:{:?}:{}", code, normalize_volatile_runs(stderr))
        }
        other => normalize_volatile_runs(&other.to_string()),
    }
}

/// Replaces runs of 7+ hex digits (commit OIDs, pids, timestamps) with `#` so
/// otherwise-identical errors that embed per-invocation values compare equal.
fn normalize_volatile_runs(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut run = String::new();
    let flush = |out: &mut String, run: &mut String| {
        if run.len() >= 7 {
            out.push('#');
        } else {
            out.push_str(run);
        }
        run.clear();
    };
    for c in text.chars() {
        if c.is_ascii_hexdigit() {
            run.push(c);
        } else {
            flush(&mut out, &mut run);
            out.push(c);
        }
    }
    flush(&mut out, &mut run);
    out
}

/// Stderr fragments git emits when the repository directory vanished between
/// trace ingestion and async processing.
const VANISHED_REPO_GIT_STDERR_MARKERS: &[&str] =
    &["cannot change to", "not a git repository", "failed to stat"];

/// True for the error `discover_repository_paths_no_git_exec` returns when a
/// traced command ran outside any repository, or in one deleted before async
/// processing.
pub(crate) fn is_repo_discovery_miss_without_exec(error: &GitAiError) -> bool {
    matches!(
        error,
        GitAiError::Generic(message)
            if message.starts_with(crate::git::repository::NO_REPO_WITHOUT_EXEC_ERROR_PREFIX)
    )
}

/// True when the error is an expected consequence of the repository being
/// removed before async processing: a repo-discovery miss without exec, or a
/// git CLI failure whose stderr indicates a missing repository while the
/// family root no longer exists on disk.
pub(crate) fn is_expected_missing_repo_error(error: &GitAiError, family_root_exists: bool) -> bool {
    if is_repo_discovery_miss_without_exec(error) {
        return true;
    }
    if family_root_exists {
        return false;
    }
    matches!(
        error,
        GitAiError::GitCliError { stderr, .. }
            if VANISHED_REPO_GIT_STDERR_MARKERS
                .iter()
                .any(|marker| stderr.contains(marker))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_cli_error(stderr: &str) -> GitAiError {
        git_cli_error_with_args(stderr, &["rev-parse"])
    }

    fn git_cli_error_with_args(stderr: &str, args: &[&str]) -> GitAiError {
        GitAiError::GitCliError {
            code: Some(128),
            stderr: stderr.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn generic(message: &str) -> GitAiError {
        GitAiError::Generic(message.to_string())
    }

    fn discovery_miss_error() -> GitAiError {
        GitAiError::Generic(format!(
            "{}: /tmp/gone-repo",
            crate::git::repository::NO_REPO_WITHOUT_EXEC_ERROR_PREFIX
        ))
    }

    #[test]
    fn first_identical_errors_log_at_error_level() {
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        for _ in 0..REPEATED_ERROR_LOG_LIMIT {
            assert_eq!(
                tracker.on_error(&generic("boom"), now),
                SideEffectErrorLog::Error
            );
        }
    }

    #[test]
    fn repeated_identical_errors_downgrade_to_debug() {
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        for _ in 0..REPEATED_ERROR_LOG_LIMIT {
            tracker.on_error(&generic("boom"), now);
        }
        assert_eq!(
            tracker.on_error(&generic("boom"), now),
            SideEffectErrorLog::RepeatedDebug
        );
        assert_eq!(
            tracker.on_error(&generic("boom"), now + Duration::from_secs(60)),
            SideEffectErrorLog::RepeatedDebug
        );
    }

    #[test]
    fn summary_fires_once_per_interval_with_suppressed_count() {
        let mut tracker = RepeatedErrorTracker::default();
        let start = Instant::now();
        for _ in 0..REPEATED_ERROR_LOG_LIMIT {
            tracker.on_error(&generic("boom"), start);
        }
        // Two occurrences suppressed inside the interval.
        assert_eq!(
            tracker.on_error(&generic("boom"), start + Duration::from_secs(60)),
            SideEffectErrorLog::RepeatedDebug
        );
        assert_eq!(
            tracker.on_error(&generic("boom"), start + Duration::from_secs(120)),
            SideEffectErrorLog::RepeatedDebug
        );
        // Interval elapsed: summary reports the two suppressed occurrences.
        let summary_at = start + REPEATED_ERROR_SUMMARY_INTERVAL;
        assert_eq!(
            tracker.on_error(&generic("boom"), summary_at),
            SideEffectErrorLog::RepeatedSummaryWarn {
                suppressed_count: 2
            }
        );
        // Counter reset: the next occurrence inside the new interval is
        // suppressed again, and the next summary reports only it.
        assert_eq!(
            tracker.on_error(&generic("boom"), summary_at + Duration::from_secs(60)),
            SideEffectErrorLog::RepeatedDebug
        );
        assert_eq!(
            tracker.on_error(
                &generic("boom"),
                summary_at + REPEATED_ERROR_SUMMARY_INTERVAL
            ),
            SideEffectErrorLog::RepeatedSummaryWarn {
                suppressed_count: 1
            }
        );
    }

    #[test]
    fn different_error_resets_suppression() {
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        for _ in 0..=REPEATED_ERROR_LOG_LIMIT {
            tracker.on_error(&generic("boom"), now);
        }
        assert_eq!(
            tracker.on_error(&generic("other"), now),
            SideEffectErrorLog::Error
        );
        // And the original error starts a fresh run too.
        assert_eq!(
            tracker.on_error(&generic("boom"), now),
            SideEffectErrorLog::Error
        );
    }

    #[test]
    fn git_cli_errors_with_differing_argv_are_identical() {
        // A persistently broken environment fails commands whose argv embeds
        // per-invocation values (OIDs, paths); suppression must still engage.
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        let stderr = "fatal: unable to write new index file";
        for i in 0..REPEATED_ERROR_LOG_LIMIT {
            let error =
                git_cli_error_with_args(stderr, &["notes", "append", &format!("oid{i}aaaaaa")]);
            assert_eq!(tracker.on_error(&error, now), SideEffectErrorLog::Error);
        }
        let error = git_cli_error_with_args(stderr, &["notes", "append", "1234567abcdef"]);
        assert_eq!(
            tracker.on_error(&error, now),
            SideEffectErrorLog::RepeatedDebug
        );
    }

    #[test]
    fn volatile_hex_runs_in_error_text_are_normalized() {
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        for oid in ["1234567890abcdef", "fedcba0987654321", "0123456aabbccdd"] {
            assert_eq!(
                tracker.on_error(&git_cli_error(&format!("fatal: bad object {oid}")), now),
                SideEffectErrorLog::Error
            );
        }
        assert_eq!(
            tracker.on_error(&git_cli_error("fatal: bad object aaaaaaabbbbbbb"), now),
            SideEffectErrorLog::RepeatedDebug
        );
        // Short hex-like words are preserved, so distinct errors stay distinct.
        assert_eq!(
            tracker.on_error(&git_cli_error("fatal: bad ref abc"), now),
            SideEffectErrorLog::Error
        );
    }

    #[test]
    fn tracker_staleness_follows_idle_eviction_window() {
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        assert!(tracker.is_stale(now));
        tracker.on_error(&generic("boom"), now);
        assert!(!tracker.is_stale(now + TRACKER_IDLE_EVICTION / 2));
        assert!(tracker.is_stale(now + TRACKER_IDLE_EVICTION));
    }

    #[test]
    fn discovery_miss_is_expected_regardless_of_root() {
        assert!(is_repo_discovery_miss_without_exec(&discovery_miss_error()));
        assert!(is_expected_missing_repo_error(
            &discovery_miss_error(),
            true
        ));
        assert!(is_expected_missing_repo_error(
            &discovery_miss_error(),
            false
        ));
    }

    #[test]
    fn vanished_repo_git_errors_are_expected_only_when_root_is_gone() {
        for stderr in [
            "fatal: cannot change to '/tmp/gone': No such file or directory",
            "fatal: not a git repository (or any of the parent directories): .git",
            "fatal: failed to stat '/tmp/gone': No such file or directory",
        ] {
            let error = git_cli_error(stderr);
            assert!(is_expected_missing_repo_error(&error, false), "{stderr}");
            assert!(!is_expected_missing_repo_error(&error, true), "{stderr}");
        }
    }

    #[test]
    fn genuine_errors_are_not_expected() {
        let merge_conflict =
            git_cli_error("error: Your local changes to the following files would be overwritten");
        assert!(!is_expected_missing_repo_error(&merge_conflict, false));
        assert!(!is_expected_missing_repo_error(&merge_conflict, true));

        let generic = generic("side effect panic");
        assert!(!is_repo_discovery_miss_without_exec(&generic));
        assert!(!is_expected_missing_repo_error(&generic, false));
    }

    #[test]
    fn expected_condition_does_not_consume_the_error_budget() {
        // Mirrors the daemon call site: expected repo-gone errors short-circuit
        // before the tracker, so genuine errors still get their loud run.
        let mut tracker = RepeatedErrorTracker::default();
        let now = Instant::now();
        let gone = git_cli_error("fatal: cannot change to '/tmp/gone': No such file or directory");
        for _ in 0..10 {
            assert!(is_expected_missing_repo_error(&gone, false));
        }
        let genuine = git_cli_error("fatal: bad object HEAD");
        assert!(!is_expected_missing_repo_error(&genuine, false));
        assert_eq!(tracker.on_error(&genuine, now), SideEffectErrorLog::Error);
    }
}
