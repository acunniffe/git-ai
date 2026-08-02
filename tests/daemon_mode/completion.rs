use super::*;

#[test]
fn await_waits_for_metrics_and_notes_flush() {
    let mut mock_api = MockApiServer::start();

    // Metrics recording is gated in test builds; point it at an isolated DB so
    // post-commit metric events actually get stored and flushed.
    let metrics_db_path =
        std::env::temp_dir().join(format!("git-ai-test-metrics-{}.db", std::process::id()));
    let mut repo = TestRepo::new_with_daemon_env(&[
        ("GIT_AI_API_BASE_URL", mock_api.base_url()),
        ("GIT_AI_API_KEY", "test-api-key"),
        ("GIT_AI_NOTES_BACKEND_KIND", "http"),
        ("GIT_AI_NOTES_BACKEND_URL", mock_api.base_url()),
        (
            "GIT_AI_TEST_METRICS_DB_PATH",
            metrics_db_path.to_str().unwrap(),
        ),
    ]);
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
        patch.prompt_storage = Some("default".to_string());
        // This test exercises upload delivery; opt into the master telemetry
        // switch (off by default) while keeping Sentry/PostHog OSS paths off.
        patch.telemetry = Some("on".to_string());
        patch.telemetry_oss_disabled = Some(true);
        patch.notes_backend = Some(NotesBackendConfig {
            kind: NotesBackendKind::Http,
            backend_url: Some(mock_api.base_url().to_string()),
        });
    });

    let repo_root = repo.canonical_path();
    let file_path = repo_root.join("test.ts");

    // First commit: known-human baseline, then an AI-style edit to produce metrics.
    fs::write(&file_path, "const x = 1;\n").expect("failed to write initial file");
    repo.git_ai(&["checkpoint", "mock_known_human", "test.ts"])
        .expect("known-human checkpoint should succeed");
    fs::write(&file_path, "const x = 2;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_ai", "test.ts"])
        .expect("ai checkpoint should succeed");
    repo.git(&["add", "-A"])
        .expect("initial add should succeed");
    repo.git(&["commit", "-m", "Initial commit"])
        .expect("initial commit should succeed");

    // Second commit: repeat the same pattern to queue more metrics and notes.
    fs::write(&file_path, "const x = 3;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_known_human", "test.ts"])
        .expect("known-human checkpoint should succeed");
    fs::write(&file_path, "const x = 4;\n").expect("failed to write update");
    repo.git_ai(&["checkpoint", "mock_ai", "test.ts"])
        .expect("ai checkpoint should succeed");
    repo.git(&["add", "-A"]).expect("second add should succeed");
    repo.git(&["commit", "-m", "Second commit"])
        .expect("second commit should succeed");

    // Wait for the daemon to finish and flush telemetry.
    let output = repo
        .git_ai(&["await", "--timeout", "30"])
        .expect("await should succeed");
    assert!(
        output.contains("finished"),
        "await should report finished: {}",
        output
    );

    let requests = mock_api.collect_requests();
    let metrics_requests = requests
        .iter()
        .filter(|r| r["path"].as_str() == Some("/worker/metrics/upload"))
        .count();
    let notes_requests = requests
        .iter()
        .filter(|r| r["path"].as_str() == Some("/worker/notes/upload"))
        .count();
    assert!(
        metrics_requests > 0,
        "expected at least one metrics upload, got {}",
        metrics_requests
    );
    assert!(
        notes_requests > 0,
        "expected at least one notes upload, got {}",
        notes_requests
    );
}

#[test]
fn daemon_debug_logging_does_not_reupload_ureq_logs() {
    let mut mock_api = MockApiServer::start();
    // Apply telemetry=on before the daemon starts so the daemon log layer sees
    // telemetry enabled from its very first tracing event (the fork gates daemon
    // log capture on telemetry_enabled(), unlike upstream).
    let repo = TestRepo::new_with_daemon_env_and_patch(
        &[
            ("RUST_LOG", "debug"),
            ("GIT_AI_API_BASE_URL", mock_api.base_url()),
            ("GIT_AI_API_KEY", "test-api-key"),
        ],
        |patch| {
            patch.telemetry = Some("on".to_string());
            patch.telemetry_oss_disabled = Some(true);
        },
    );

    repo.git_ai(&["await", "--timeout", "10"])
        .expect("initial daemon log flush should succeed");

    let first_upload_deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut requests = Vec::new();
    while std::time::Instant::now() < first_upload_deadline {
        requests.extend(mock_api.collect_requests());
        if requests
            .iter()
            .any(|request| request["path"] == "/worker/logs/upload")
        {
            break;
        }
        thread::sleep(Duration::from_millis(25));
    }

    thread::sleep(Duration::from_millis(250));
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("follow-up daemon log flush should succeed");
    thread::sleep(Duration::from_millis(250));
    requests.extend(mock_api.collect_requests());

    let uploaded_targets = requests
        .iter()
        .filter(|request| request["path"] == "/worker/logs/upload")
        .flat_map(|request| request["body"]["events"].as_array().into_iter().flatten())
        .filter_map(|event| {
            event["fields"]["log.target"]
                .as_str()
                .or_else(|| event["target"].as_str())
        })
        .collect::<Vec<_>>();

    assert!(
        !uploaded_targets.is_empty(),
        "expected the daemon to upload its startup logs"
    );
    assert!(
        uploaded_targets
            .iter()
            .all(|target| *target != "ureq" && !target.starts_with("ureq::")),
        "ureq logs generated by daemon log delivery must not be uploaded: {uploaded_targets:?}"
    );
}

#[test]
fn await_is_marked_beta_and_returns_promptly_when_idle() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let top_level_help = repo
        .git_ai(&["--help"])
        .expect("top-level help should succeed");
    assert!(
        top_level_help.contains("await [beta]"),
        "top-level help should mark await as beta: {}",
        top_level_help
    );

    let await_help = repo
        .git_ai(&["await", "--help"])
        .expect("await help should succeed");
    assert!(
        await_help.contains("beta"),
        "await help should mark the command as beta: {}",
        await_help
    );

    let started_at = std::time::Instant::now();
    repo.git_ai(&["await", "--timeout", "10"])
        .expect("await should succeed when the daemon is idle");
    assert!(
        started_at.elapsed() < Duration::from_secs(4),
        "await should return promptly instead of waiting for the progress interval"
    );
}

#[test]
fn await_rejects_zero_timeout() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::Dedicated);

    let error = repo
        .git_ai(&["await", "--timeout", "0"])
        .expect_err("zero timeout should be rejected");

    assert!(
        error.contains("--timeout must be a positive integer"),
        "await should report an input validation error: {error}"
    );
}

// -----------------------------------------------------------------------
// Completion log diagnostics: `semantic_events` / `commit_shas` /
// `commit_skip_reason` on `TestCompletionLogEntry`.
//
// These close the diagnostic gap behind the recurring
// "No authorship log found for new commit <sha> after daemon sync" flake:
// a commit whose RefCursor reflog-cursor capture lost the race with git's
// own reflog append gets no ref enrichment, so HistoryAnalyzer classifies
// it as `OpaqueCommand` and `handle_commit_created` never runs -- yet the
// completion entry still reports status "ok" (the command was processed
// without error; it just produced no commit-shaping event). The fields
// asserted here let `commit_with_env` (test_repo.rs) tell that apart from a
// pure filesystem-visibility lag instead of retrying for 500ms and failing
// with a generic message.
// -----------------------------------------------------------------------

#[test]
fn commit_completion_entry_reports_commit_created_event_and_note_sha() {
    let repo = TestRepo::new();
    repo.filename("completion-log-normal.txt")
        .set_contents(lines!["first line"]);

    let new_commit = repo
        .stage_all_and_commit("normal commit for completion log diagnostics")
        .expect("commit should succeed and produce an authorship note");

    let commit_entries = completion_entries_for_command(&repo, "commit");
    let entry = commit_entries
        .iter()
        .find(|entry| entry.commit_shas.contains(&new_commit.commit_sha))
        .unwrap_or_else(|| {
            panic!(
                "expected a commit completion entry reporting sha {} among {:?}",
                new_commit.commit_sha, commit_entries
            )
        });
    assert!(
        entry.semantic_events.contains(&"CommitCreated".to_string()),
        "expected CommitCreated in semantic_events, got {:?}",
        entry.semantic_events
    );
    assert_eq!(
        entry.commit_skip_reason, None,
        "a successful commit that produced a note must not carry a skip reason: {:?}",
        entry
    );
}

#[test]
fn branch_completion_entry_has_no_commit_shas_or_skip_reason() {
    let repo = TestRepo::new();
    repo.filename("completion-log-branch-seed.txt")
        .set_contents(lines!["seed"]);
    repo.stage_all_and_commit("seed commit for branch completion log test")
        .expect("seed commit should succeed");

    repo.git(&["branch", "completion-log-side-branch"])
        .expect("git branch should succeed");
    repo.sync_daemon();

    let branch_entries = completion_entries_for_command(&repo, "branch");
    assert!(
        !branch_entries.is_empty(),
        "expected at least one tracked completion entry for git branch"
    );
    for entry in &branch_entries {
        assert!(
            entry.commit_shas.is_empty(),
            "a branch command must never report commit SHAs: {:?}",
            entry
        );
        assert_eq!(
            entry.commit_skip_reason, None,
            "non-commit commands never carry a skip reason (daemon gates it to commit-family): {:?}",
            entry
        );
    }
}
