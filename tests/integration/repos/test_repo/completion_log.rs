use super::*;

type Progress = (u64, usize);

pub(super) fn advance_poll(
    last: &mut (Instant, Progress),
    next: Progress,
    now: impl FnOnce() -> Instant,
) {
    if next.0 > last.1.0 || next.1 > last.1.1 {
        *last = (now(), next);
    }
}

pub(super) fn poll_timed_out(
    start: Instant,
    progress: Instant,
    mut now: impl FnMut() -> Instant,
) -> bool {
    now().duration_since(start) >= DAEMON_TEST_SYNC_TOTAL_TIMEOUT
        || now().duration_since(progress) >= DAEMON_TEST_SYNC_IDLE_TIMEOUT
}

#[derive(Debug, Clone, serde::Deserialize)]
pub(crate) struct DaemonTestCompletionLogEntry {
    #[serde(default)]
    pub(crate) seq: u64,
    #[serde(default)]
    pub(crate) kind: String,
    #[serde(default)]
    pub(crate) primary_command: Option<String>,
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    #[serde(default)]
    pub(crate) sync_tracked: bool,
    #[serde(default)]
    pub(crate) test_sync_session: Option<String>,
    pub(crate) status: String,
    pub(crate) error: Option<String>,
    /// Semantic event kinds the daemon's analyzer produced for this command
    /// (e.g. `"CommitCreated"`, `"OpaqueCommand"`). Empty for entries from a
    /// daemon binary predating this field -- `#[serde(default)]` so older
    /// completion log lines still parse.
    #[serde(default)]
    pub(crate) semantic_events: Vec<String>,
    /// `new_head` SHAs from `CommitCreated`/`CommitAmended` events seen for
    /// this command. See `commit_skip_reason` for why this can be empty.
    #[serde(default)]
    pub(crate) commit_shas: Vec<String>,
    /// `Some("opaque_command")` when the daemon's analyzer produced only an
    /// `OpaqueCommand` event for this command -- see the doc comment on
    /// `TestCompletionLogEntry` in `daemon_config.rs` for what that means.
    #[serde(default)]
    pub(crate) commit_skip_reason: Option<String>,
}

/// Diagnostic check run by `commit_with_env` after `sync_daemon_force()` has
/// drained a commit's completion session (see `TestRepo::
/// fail_fast_on_opaque_commit_completion`, its only caller). Pure over
/// already-fetched `entries` so the classification logic is unit-testable
/// without spinning up a daemon or reproducing the reflog-cursor race that
/// motivated it.
///
/// Looks at the entries appended since `baseline` for the last one that
/// classified this `git commit` invocation (`kind == "command"`,
/// `primary_command == "commit"`). Fails immediately, carrying the daemon's
/// own classification, when that entry is new-format (non-empty
/// `semantic_events`) but reports no `CommitCreated`/`CommitAmended` SHA --
/// no amount of retrying makes a note appear for a commit the analyzer never
/// produced a HEAD-transition event for. A reported SHA only satisfies the
/// diagnostic when it matches `head_commit`; a different SHA means the
/// completion entry belongs to another command/commit and is equally unable
/// to prove that this commit's note generation was attempted. Falls through
/// to `Ok(())` (the generic fs-visibility retry in `commit_with_env`) when no
/// matching entry is found or when it predates this diagnostic
/// (`semantic_events` empty) and we therefore cannot tell.
pub(super) fn commit_completion_diagnostic(
    entries: &[DaemonTestCompletionLogEntry],
    head_commit: &str,
    baseline: usize,
) -> Result<(), String> {
    let Some(entry) = entries.iter().skip(baseline).rev().find(|entry| {
        entry.kind == "command" && entry.primary_command.as_deref() == Some("commit")
    }) else {
        return Ok(());
    };
    if entry.semantic_events.is_empty() || entry.commit_shas.iter().any(|sha| sha == head_commit) {
        return Ok(());
    }
    let reason = entry
        .commit_skip_reason
        .as_deref()
        .unwrap_or(if entry.commit_shas.is_empty() {
            "no_commit_event"
        } else {
            "commit_sha_mismatch"
        });
    Err(format!(
        "daemon processed commit {head_commit} as {reason} (analyzer events: {:?}, \
         reported commit SHAs: {:?}, completion entry: {:?}) -- \
         no note was or will be generated for it. The usual cause is the reflog-cursor \
         race: RefCursor enrichment lost the race with git's own reflog append, so \
         HistoryAnalyzer saw no HEAD transition and emitted OpaqueCommand instead of \
         CommitCreated, and handle_commit_created never ran. This is NOT a filesystem-\
         visibility delay -- retrying will not help. See CLAUDE.md's daemon trace2 \
         ingestion notes and docs/architecture/daemon-trace2-ingestion-spec.md.",
        entry.semantic_events, entry.commit_shas, entry
    ))
}

impl TestRepo {
    pub(super) fn write_daemon_completion_log_fixture(&self, family_key: &str, contents: &str) {
        let path = self.daemon_completion_log_path_for_family(family_key);
        let parent = path
            .parent()
            .expect("daemon completion log path should have a parent");
        fs::create_dir_all(parent).unwrap_or_else(|error| {
            panic!(
                "failed to create daemon completion log fixture directory {}: {}",
                parent.display(),
                error
            )
        });
        fs::write(&path, contents).unwrap_or_else(|error| {
            panic!(
                "failed to write daemon completion log fixture {}: {}",
                path.display(),
                error
            )
        });
    }

    pub(super) fn daemon_completion_log_path_for_family(&self, family_key: &str) -> PathBuf {
        DaemonConfig::from_home(&self.daemon_home_path())
            .test_completion_log_path_for_family(family_key)
    }

    pub(crate) fn daemon_total_completion_count(&self) -> u64 {
        let family_key = self.daemon_family_key();
        self.daemon_completion_entries_for_family(&family_key).len() as u64
    }

    pub(crate) fn daemon_completion_entries(&self) -> Vec<DaemonTestCompletionLogEntry> {
        let family_key = self.daemon_family_key();
        self.daemon_completion_entries_for_family(&family_key)
    }

    pub(super) fn daemon_completion_entries_for_family(
        &self,
        family_key: &str,
    ) -> Vec<DaemonTestCompletionLogEntry> {
        let path = self.daemon_completion_log_path_for_family(family_key);
        let Ok(content) = fs::read_to_string(&path) else {
            return Vec::new();
        };
        let ends_with_newline = content.ends_with('\n');
        let lines: Vec<&str> = content
            .lines()
            .filter(|line| !line.trim().is_empty())
            .collect();
        let total_lines = lines.len();

        lines
            .into_iter()
            .enumerate()
            .filter_map(|(idx, line)| {
                match serde_json::from_str::<DaemonTestCompletionLogEntry>(line) {
                    Ok(entry) => Some(entry),
                    Err(error)
                        if idx + 1 == total_lines && !ends_with_newline && error.is_eof() =>
                    {
                        None
                    }
                    Err(error) => {
                        panic!(
                            "failed to parse daemon completion log entry {} in {}: {}",
                            idx + 1,
                            path.display(),
                            error
                        )
                    }
                }
            })
            .collect()
    }

    pub(super) fn poll_daemon_completion_log(
        &self,
        family_key: &str,
        initial_observed: u64,
        mut inspect: impl FnMut(&[DaemonTestCompletionLogEntry]) -> (bool, u64, usize),
    ) -> Result<u64, u64> {
        let started_at = Instant::now();
        let mut last_progress = (started_at, (initial_observed, 0));
        loop {
            let entries = self.daemon_completion_entries_for_family(family_key);
            let (ready, observed, matched) = inspect(&entries);
            if ready {
                return Ok(observed);
            }
            advance_poll(&mut last_progress, (observed, matched), Instant::now);
            if poll_timed_out(started_at, last_progress.0, Instant::now) {
                return Err(last_progress.1.0);
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    pub(super) fn wait_for_daemon_completion_count(
        &self,
        family_key: &str,
        baseline_count: u64,
        expected_count: u64,
    ) -> u64 {
        self.poll_daemon_completion_log(family_key, baseline_count, |entries| {
            let tracked_entries = entries.iter().filter(|entry| entry.sync_tracked);
            if let Some(error_entry) = tracked_entries
                .clone()
                .skip(baseline_count as usize)
                .find(|entry| entry.status == "error")
            {
                let error = error_entry
                    .error
                    .as_deref()
                    .unwrap_or("unknown completion error");
                panic!(
                    "daemon completion log reported an error for family {}: {}",
                    family_key, error
                );
            }
            let observed = tracked_entries.count() as u64;
            (observed >= expected_count, observed, 0)
        })
        .unwrap_or_else(|_| {
            panic!(
                "daemon completion log for family {} did not reach {} entries within timeout",
                family_key, expected_count
            )
        })
    }

    pub(super) fn wait_for_daemon_checkpoint_count(
        &self,
        family_key: &str,
        expected_checkpoint_count: u64,
    ) -> u64 {
        self.poll_daemon_completion_log(family_key, 0, |entries| {
            let checkpoints =
                entries.iter().filter(|e| e.sync_tracked && e.kind == "checkpoint");
            if let Some(error_entry) = checkpoints.clone().find(|e| e.status == "error") {
                let error = error_entry
                    .error
                    .as_deref()
                    .unwrap_or("unknown checkpoint error");
                panic!(
                    "daemon checkpoint completion reported an error for family {}: {}",
                    family_key, error
                );
            }
            let observed = checkpoints.count() as u64;
            (observed >= expected_checkpoint_count, observed, 0)
        })
        .unwrap_or_else(|last_observed| {
            panic!(
                "daemon checkpoint completions for family {} did not reach {} within timeout (observed {})",
                family_key, expected_checkpoint_count, last_observed
            )
        })
    }

    pub(super) fn wait_for_daemon_completion_sessions(
        &self,
        family_key: &str,
        sessions: &[String],
    ) -> u64 {
        let expected: HashSet<&str> = sessions.iter().map(String::as_str).collect();
        self.poll_daemon_completion_log(family_key, 0, |entries| {
            let tracked_entries = entries.iter().filter(|entry| entry.sync_tracked);
            let mut completed = HashSet::<&str>::new();

            for entry in tracked_entries.clone() {
                let Some(session) = entry.test_sync_session.as_deref() else {
                    continue;
                };
                if !expected.contains(session) {
                    continue;
                }
                if entry.status == "error" {
                    panic!(
                        "daemon completion log reported an error for family {} session {}: {}",
                        family_key,
                        session,
                        entry.error.as_deref().unwrap_or("unknown completion error")
                    );
                }
                completed.insert(session);
            }

            let observed = tracked_entries.count() as u64;
            (completed.len() == expected.len(), observed, completed.len())
        })
        .unwrap_or_else(|_| {
            panic!(
                "daemon completion log for family {} did not observe all sessions within timeout: {:?}",
                family_key, sessions
            )
        })
    }

    pub(crate) fn wait_for_daemon_total_completion_count(
        &self,
        baseline_count: u64,
        expected_count: u64,
    ) -> u64 {
        let family_key = self.daemon_family_key();
        if let Ok(observed) =
            self.poll_daemon_completion_log(&family_key, baseline_count, |entries| {
                if let Some(error_entry) = entries
                    .iter()
                    .skip(baseline_count as usize)
                    .find(|entry| entry.status == "error")
                {
                    let error = error_entry
                        .error
                        .as_deref()
                        .unwrap_or("unknown completion error");
                    panic!(
                        "daemon completion log reported an error for family {}: {}",
                        family_key, error
                    );
                }
                let observed = entries.len() as u64;
                (observed >= expected_count, observed, 0)
            })
        {
            return observed;
        }

        let final_entries = self.daemon_completion_entries_for_family(&family_key);
        let observed_count = final_entries.len() as u64;
        let recent_entries = final_entries
            .iter()
            .rev()
            .take(5)
            .map(|entry| format!("{}:{:?}:{}", entry.seq, entry.primary_command, entry.status))
            .collect::<Vec<_>>();

        panic!(
            "daemon completion log for family {} did not reach {} total entries within timeout (observed {}, recent entries {:?})",
            family_key, expected_count, observed_count, recent_entries
        );
    }

    pub(crate) fn wait_for_next_daemon_checkpoint_completion(&self, baseline_count: u64) -> u64 {
        self.wait_for_daemon_total_completion_count(
            baseline_count,
            baseline_count.saturating_add(1),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_log_polling_preserves_progress_and_deadline_boundaries() {
        let start = Instant::now();
        let progress = start + Duration::from_millis(10);
        let idle = DAEMON_TEST_SYNC_IDLE_TIMEOUT;
        let mut last = (start, (0, 0));
        advance_poll(&mut last, (1, 0), || progress);
        advance_poll(&mut last, (1, 1), || progress + Duration::from_millis(10));
        advance_poll(&mut last, (1, 1), || panic!("equal progress reset idle"));
        assert_eq!(last, (progress + Duration::from_millis(10), (1, 1)));
        assert!(!poll_timed_out(start, last.0, || last.0 + idle / 2));
        assert!(poll_timed_out(start, last.0, || last.0 + idle));
        assert!(poll_timed_out(start, start, || start + DAEMON_TEST_SYNC_TOTAL_TIMEOUT));
    }

    #[test]
    fn completion_log_waiters_preserve_caller_owned_policies() {
        let repo = TestRepo::new();
        let family = repo.daemon_family_key();
        let log = "{\"status\":\"ok\"}\n{\"status\":\"error\",\"sync_tracked\":true,\"test_sync_session\":\"other\",\"error\":\"before baseline\"}\n{\"kind\":\"checkpoint\",\"status\":\"ok\",\"sync_tracked\":true,\"test_sync_session\":\"wanted\"}\n";
        repo.write_daemon_completion_log_fixture(&family, log);
        assert_eq!(repo.wait_for_daemon_total_completion_count(2, 3), 3);
        assert_eq!(repo.wait_for_daemon_completion_count(&family, 1, 2), 2);
        assert_eq!(repo.wait_for_daemon_checkpoint_count(&family, 1), 1);
        assert_eq!(
            repo.wait_for_daemon_completion_sessions(&family, &["wanted".to_string()]),
            2
        );
        let wait = || repo.wait_for_daemon_completion_count(&family, 0, 2);
        assert!(std::panic::catch_unwind(wait).is_err());
    }

    #[test]
    fn completion_log_fixture_creates_missing_parent_directory() {
        let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
        let family = repo.daemon_family_key();
        let path = repo.daemon_completion_log_path_for_family(&family);
        let parent = path.parent().expect("completion log should have a parent");
        if parent.exists() {
            fs::remove_dir_all(parent).expect("completion log parent should be removable");
        }

        repo.write_daemon_completion_log_fixture(&family, "{\"status\":\"ok\"}\n");

        assert!(parent.is_dir());
        assert_eq!(fs::read_to_string(path).unwrap(), "{\"status\":\"ok\"}\n");
    }

    fn completion_entry(
        primary_command: &str,
        semantic_events: Vec<&str>,
        commit_shas: Vec<&str>,
        commit_skip_reason: Option<&str>,
    ) -> DaemonTestCompletionLogEntry {
        DaemonTestCompletionLogEntry {
            seq: 1,
            kind: "command".to_string(),
            primary_command: Some(primary_command.to_string()),
            exit_code: Some(0),
            sync_tracked: true,
            test_sync_session: None,
            status: "ok".to_string(),
            error: None,
            semantic_events: semantic_events.into_iter().map(String::from).collect(),
            commit_shas: commit_shas.into_iter().map(String::from).collect(),
            commit_skip_reason: commit_skip_reason.map(String::from),
        }
    }

    #[test]
    fn commit_completion_diagnostic_fails_fast_on_opaque_commit_classification() {
        let entries = vec![completion_entry(
            "commit",
            vec!["OpaqueCommand"],
            vec![],
            Some("opaque_command"),
        )];

        let error = commit_completion_diagnostic(&entries, "deadbeef", 0)
            .expect_err("an opaque-command classification must fail fast, not retry");
        assert!(
            error.contains("opaque_command"),
            "error should carry the daemon's own classification: {error}"
        );
        assert!(
            error.contains("deadbeef"),
            "error should name the commit sha it was diagnosing: {error}"
        );
    }

    #[test]
    fn commit_completion_diagnostic_passes_when_commit_sha_is_reported() {
        let entries = vec![completion_entry(
            "commit",
            vec!["CommitCreated"],
            vec!["deadbeef"],
            None,
        )];

        assert_eq!(
            commit_completion_diagnostic(&entries, "deadbeef", 0),
            Ok(()),
            "a matching commit_shas entry means note generation was attempted; \
             any remaining gap is a pure fs-visibility wait for the retry loop"
        );
    }

    #[test]
    fn commit_completion_diagnostic_fails_when_reported_sha_does_not_match_head() {
        let entries = vec![completion_entry(
            "commit",
            vec!["CommitCreated"],
            vec!["another-commit"],
            None,
        )];

        let error = commit_completion_diagnostic(&entries, "deadbeef", 0)
            .expect_err("a completion entry for another commit must not satisfy this commit");
        assert!(
            error.contains("deadbeef"),
            "error should name the commit sha it was diagnosing: {error}"
        );
        assert!(
            error.contains("another-commit"),
            "error should include the daemon-reported commit shas: {error}"
        );
    }

    #[test]
    fn commit_completion_diagnostic_falls_through_for_pre_migration_entries() {
        // An entry from a daemon binary predating this diagnostic has no
        // semantic_events at all (serde defaults it to empty). We cannot
        // distinguish "genuinely opaque" from "old format" here, so we must
        // not fail fast -- that would be a false positive against an older
        // daemon replaying from the shared test pool.
        let entries = vec![completion_entry("commit", vec![], vec![], None)];

        assert_eq!(
            commit_completion_diagnostic(&entries, "deadbeef", 0),
            Ok(())
        );
    }

    #[test]
    fn commit_completion_diagnostic_falls_through_when_no_matching_entry_exists() {
        let entries = vec![completion_entry(
            "branch",
            vec!["BranchCreated"],
            vec![],
            None,
        )];

        assert_eq!(
            commit_completion_diagnostic(&entries, "deadbeef", 0),
            Ok(())
        );
    }

    #[test]
    fn commit_completion_diagnostic_only_inspects_entries_after_baseline() {
        // An opaque commit entry that predates this specific commit attempt
        // (e.g. left over from an earlier commit_with_env call the caller
        // already resolved) must not leak into this diagnosis.
        let entries = vec![
            completion_entry(
                "commit",
                vec!["OpaqueCommand"],
                vec![],
                Some("opaque_command"),
            ),
            completion_entry("commit", vec!["CommitCreated"], vec!["deadbeef"], None),
        ];

        assert_eq!(
            commit_completion_diagnostic(&entries, "deadbeef", 1),
            Ok(())
        );
    }
}
