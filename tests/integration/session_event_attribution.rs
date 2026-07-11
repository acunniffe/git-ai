use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::TestRepo;
use git_ai::authorship::authorship_log::LineRange;
use git_ai::authorship::authorship_log_serialization::{AuthorshipLog, generate_session_id};
use git_ai::authorship::working_log::AgentId;
use git_ai::daemon::bash_history_db::{BashCallEnd, BashCallStart, BashHistoryDatabase};
use git_ai::metrics::db::MetricsDatabase;
use git_ai::metrics::{EventAttributes, MetricEvent, PosEncoded, SessionEventValues};
use serde_json::json;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::time::UNIX_EPOCH;

fn isolated_metrics_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated metrics db dir");
    let path = dir.path().join("metrics.db");
    (dir, path.to_string_lossy().to_string())
}

fn file_mtime_secs(path: &Path) -> u32 {
    fs::metadata(path)
        .expect("file metadata should be readable")
        .modified()
        .expect("file mtime should be readable")
        .duration_since(UNIX_EPOCH)
        .expect("file mtime should be after epoch")
        .as_secs()
        .min(u32::MAX as u64) as u32
}

fn insert_session_event(
    db_path: &str,
    event_ts: u32,
    external_session_id: &str,
    external_tool_use_id: &str,
    repo_url: Option<&str>,
) -> String {
    insert_session_event_for_tool(
        db_path,
        event_ts,
        "codex",
        external_session_id,
        external_tool_use_id,
        repo_url,
    )
}

fn insert_session_event_for_tool(
    db_path: &str,
    event_ts: u32,
    tool: &str,
    external_session_id: &str,
    external_tool_use_id: &str,
    repo_url: Option<&str>,
) -> String {
    let session_id = generate_session_id(external_session_id, tool);
    let values = SessionEventValues::with_ids(
        json!({
            "type": "assistant",
            "session_id": external_session_id,
        }),
        Some(format!("event-{external_tool_use_id}")),
        None,
        Some(external_tool_use_id.to_string()),
    );
    let mut attrs = EventAttributes::with_version("test")
        .tool(tool)
        .model("gpt-5")
        .external_session_id(external_session_id)
        .session_id(&session_id)
        .trace_id(format!("trace-{external_tool_use_id}"));
    if let Some(repo_url) = repo_url {
        attrs = attrs.repo_url(repo_url);
    }
    let event = MetricEvent::from_values_with_timestamp(values, attrs.to_sparse(), Some(event_ts));
    let event_json = serde_json::to_string(&event).expect("metric event should serialize");

    let mut db = MetricsDatabase::open_at_path(Path::new(db_path))
        .expect("metrics db should open at isolated path");
    db.insert_events(&[event_json])
        .expect("session event should insert");

    session_id
}

fn codex_checkpoint(
    repo: &TestRepo,
    file_path: &Path,
    session_id: &str,
    hook_event_name: &str,
    tool_use_id: &str,
) {
    let hook_input = json!({
        "session_id": session_id,
        "cwd": repo.canonical_path().to_string_lossy().to_string(),
        "hook_event_name": hook_event_name,
        "tool_name": "apply_patch",
        "tool_use_id": tool_use_id,
        "model": "gpt-5",
        "tool_input": {
            "patch": format!("*** Update File: {}\n", file_path.to_string_lossy())
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "codex", "--hook-input", &hook_input])
        .expect("codex checkpoint should succeed");
}

fn attested_lines_for_session(
    authorship_log: &AuthorshipLog,
    file_path: &str,
    session_id: &str,
) -> Vec<u32> {
    let mut lines = authorship_log
        .attestations
        .iter()
        .filter(|attestation| attestation.file_path == file_path)
        .flat_map(|attestation| &attestation.entries)
        .filter(|entry| entry.hash.split("::").next() == Some(session_id))
        .flat_map(|entry| entry.line_ranges.iter().flat_map(LineRange::expand))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn assert_session_attests_lines(
    authorship_log: &AuthorshipLog,
    file_path: &str,
    session_id: &str,
    expected_lines: &[u32],
) {
    assert_eq!(
        attested_lines_for_session(authorship_log, file_path, session_id),
        expected_lines,
        "expected {session_id} to attest lines {expected_lines:?}"
    );
}

fn human_attested_lines(authorship_log: &AuthorshipLog, file_path: &str) -> Vec<u32> {
    let mut lines = authorship_log
        .attestations
        .iter()
        .filter(|attestation| attestation.file_path == file_path)
        .flat_map(|attestation| &attestation.entries)
        .filter(|entry| entry.hash.starts_with("h_"))
        .flat_map(|entry| entry.line_ranges.iter().flat_map(LineRange::expand))
        .collect::<Vec<_>>();
    lines.sort_unstable();
    lines.dedup();
    lines
}

fn assert_human_attests_lines(
    authorship_log: &AuthorshipLog,
    file_path: &str,
    expected_lines: &[u32],
) {
    assert_eq!(
        human_attested_lines(authorship_log, file_path),
        expected_lines,
        "expected known-human attestation for {file_path} lines {expected_lines:?}"
    );
}

fn session_ids_for_tool(authorship_log: &AuthorshipLog, tool: &str) -> Vec<String> {
    let mut session_ids = authorship_log
        .metadata
        .sessions
        .iter()
        .filter(|(_, session)| session.agent_id.tool == tool)
        .map(|(session_id, _)| session_id.clone())
        .collect::<Vec<_>>();
    session_ids.sort();
    session_ids
}

fn isolated_bash_history_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated bash history db dir");
    let path = dir.path().join("bash-history.db");
    (dir, path.to_string_lossy().to_string())
}

fn insert_bash_call(
    db_path: &str,
    repo_work_dir: &str,
    timestamp_secs: u32,
    external_session_id: &str,
    tool_use_id: &str,
) -> String {
    let tool = "codex";
    let session_id = generate_session_id(external_session_id, tool);
    let mut db = BashHistoryDatabase::open_at_path(Path::new(db_path))
        .expect("bash history db should open at isolated path");
    let start_ns = u128::from(timestamp_secs).saturating_mul(1_000_000_000);
    let end_ns = start_ns.saturating_add(1_000_000_000);
    let agent_id = AgentId {
        tool: tool.to_string(),
        id: external_session_id.to_string(),
        model: "gpt-5".to_string(),
    };
    db.record_start(&BashCallStart {
        original_cwd: repo_work_dir.to_string(),
        repo_work_dir: Some(repo_work_dir.to_string()),
        repo_discovery_error: None,
        session_id: external_session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
        agent_id: agent_id.clone(),
        start_trace_id: format!("trace-start-{tool_use_id}"),
        started_at_ns: start_ns,
        command: Some("codex exec".to_string()),
        metadata: HashMap::new(),
    })
    .expect("bash start should insert");
    db.record_end(&BashCallEnd {
        original_cwd: repo_work_dir.to_string(),
        repo_work_dir: Some(repo_work_dir.to_string()),
        repo_discovery_error: None,
        session_id: external_session_id.to_string(),
        tool_use_id: tool_use_id.to_string(),
        agent_id,
        start_trace_id: Some(format!("trace-start-{tool_use_id}")),
        end_trace_id: format!("trace-end-{tool_use_id}"),
        started_at_ns: Some(start_ns),
        ended_at_ns: end_ns,
        command: Some("codex exec".to_string()),
        metadata: HashMap::new(),
    })
    .expect("bash end should insert");

    session_id
}

#[test]
fn test_session_event_recovery_attributes_uncheckpointed_repo_linked_commit() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/session-event-recovery.git",
    ])
    .expect("remote add should succeed");
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("generated.txt");
    fs::write(&file_path, "generated by AI before hooks\n").unwrap();
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_mtime_secs(&file_path),
        "external-repo-linked-session",
        "tool-use-repo-linked",
        Some("https://github.com/acme/session-event-recovery"),
    );

    let commit = repo
        .stage_all_and_commit("Recover uncheckpointed AI")
        .expect("commit should succeed");

    let mut file = repo.filename("generated.txt");
    file.assert_committed_lines(lines!["generated by AI before hooks".ai()]);
    assert!(
        commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "recovered note should include the session-event session record"
    );
}

/// Root-commit coverage for the bounded recovery diff base. When the recovered
/// commit is the repo's *first* commit there is no parent, so `parent_sha` is
/// `"initial"` and `single_commit_diff_base` must fall back to the empty tree
/// (there is no `<commit>^`). Recovery must still attribute the uncheckpointed
/// AI file. Guards the `parent_sha == "initial"` branch of the PD-23 fix.
#[test]
fn test_session_event_recovery_attributes_uncheckpointed_first_commit() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/session-event-recovery.git",
    ])
    .expect("remote add should succeed");

    // No initial commit: the recovered content lands in the very first commit,
    // so post-commit sees parent_sha == "initial" (empty-tree diff base).
    let file_path = repo.path().join("generated.txt");
    fs::write(&file_path, "generated by AI before hooks\n").unwrap();
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_mtime_secs(&file_path),
        "external-first-commit-session",
        "tool-use-first-commit",
        Some("https://github.com/acme/session-event-recovery"),
    );

    let commit = repo
        .stage_all_and_commit("Recover uncheckpointed AI in first commit")
        .expect("commit should succeed");

    let mut file = repo.filename("generated.txt");
    file.assert_committed_lines(lines!["generated by AI before hooks".ai()]);
    assert!(
        commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "recovery on a first commit (empty-tree diff base) should still attribute the AI file"
    );
}

#[test]
fn test_session_event_recovery_does_not_override_nearby_bash_candidate() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/session-event-recovery.git",
    ])
    .expect("remote add should succeed");
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("generated-with-bash-noise.txt");
    fs::write(&file_path, "generated by inner codex\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_ts,
        "external-inner-session",
        "tool-use-inner",
        Some("https://github.com/acme/session-event-recovery"),
    );
    let bash_session_id = insert_bash_call(
        &bash_db_path,
        repo.canonical_path().to_string_lossy().as_ref(),
        file_ts,
        "external-outer-bash-session",
        "tool-use-outer-bash",
    );

    let commit = repo
        .stage_all_and_commit("Nearby bash attribution runs before session event")
        .expect("commit should succeed");

    let mut file = repo.filename("generated-with-bash-noise.txt");
    file.assert_committed_lines(lines!["generated by inner codex".ai()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "session-event recovery should only see lines still unknown after bash recovery"
    );
    assert!(
        commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&bash_session_id),
        "bash recovery should retain first pass over nearby bash candidates"
    );
}

#[test]
fn test_session_event_recovery_does_not_override_known_human_checkpoint() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("human.txt");
    fs::write(&file_path, "typed by a human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "human.txt"])
        .expect("known-human checkpoint should succeed");
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_mtime_secs(&file_path),
        "external-human-nearby-session",
        "tool-use-human-nearby",
        None,
    );

    let commit = repo
        .stage_all_and_commit("Known human stays human")
        .expect("commit should succeed");

    let mut file = repo.filename("human.txt");
    file.assert_committed_lines(lines!["typed by a human".human()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "nearby session event must not be used when explicit human attribution exists"
    );
}

#[test]
fn test_pure_human_recovery_marks_uncheckpointed_no_ai_commit_known_human() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("manual.txt");
    fs::write(&file_path, "manual one\nmanual two\n").unwrap();

    let commit = repo
        .stage_all_and_commit("Manual no-AI edit")
        .expect("commit should succeed");

    let mut file = repo.filename("manual.txt");
    file.assert_committed_lines(lines!["manual one".human(), "manual two".human()]);
    assert_human_attests_lines(&commit.authorship_log, "manual.txt", &[1, 2]);
}

#[test]
fn test_pure_human_recovery_marks_legacy_human_checkpoint_no_ai_commit_known_human() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("legacy.txt");
    fs::write(&file_path, "legacy one\nlegacy two\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "legacy.txt"])
        .expect("legacy human checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Legacy no-AI edit")
        .expect("commit should succeed");

    let mut file = repo.filename("legacy.txt");
    file.assert_committed_lines(lines!["legacy one".human(), "legacy two".human()]);
    assert_human_attests_lines(&commit.authorship_log, "legacy.txt", &[1, 2]);
}

#[test]
fn test_known_human_recovery_does_not_extend_current_checkpoint_in_mixed_ai_commit() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let human_path = repo.path().join("human-side.txt");
    fs::write(&human_path, "known human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "human-side.txt"])
        .expect("known-human checkpoint should succeed");

    fs::write(&human_path, "known human\nlegacy untracked\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "human-side.txt"])
        .expect("legacy human checkpoint should succeed");

    fs::write(repo.path().join("ai-side.txt"), "ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai-side.txt"])
        .expect("AI checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Known human plus AI and legacy")
        .expect("commit should succeed");

    let mut human_file = repo.filename("human-side.txt");
    human_file.assert_committed_lines(lines![
        "known human".human(),
        "legacy untracked".unattributed_human(),
    ]);
    let mut ai_file = repo.filename("ai-side.txt");
    ai_file.assert_committed_lines(lines!["ai line".ai()]);
    assert_human_attests_lines(&commit.authorship_log, "human-side.txt", &[1]);
}

#[test]
fn test_known_human_recovery_does_not_extend_parent_identity_in_mixed_ai_commit() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("same-file.txt");
    fs::write(&file_path, "known human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "same-file.txt"])
        .expect("known-human checkpoint should succeed");
    repo.stage_all_and_commit("Known human baseline")
        .expect("baseline commit should succeed");

    fs::write(&file_path, "known human\nlegacy untracked\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "same-file.txt"])
        .expect("legacy human checkpoint should succeed");

    fs::write(repo.path().join("ai-side.txt"), "ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai-side.txt"])
        .expect("AI checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Known human file with AI and legacy")
        .expect("mixed commit should succeed");

    let mut file = repo.filename("same-file.txt");
    file.assert_committed_lines(lines![
        "known human".human(),
        "legacy untracked".unattributed_human(),
    ]);
    let mut ai_file = repo.filename("ai-side.txt");
    ai_file.assert_committed_lines(lines!["ai line".ai()]);
    assert_human_attests_lines(&commit.authorship_log, "same-file.txt", &[]);
}

#[test]
fn test_known_human_recovery_does_not_extend_current_checkpoint_across_nearby_ai_signal() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("inherited-human.txt");
    fs::write(&file_path, "known human baseline\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "inherited-human.txt"])
        .expect("known-human checkpoint should succeed");

    fs::write(
        &file_path,
        "known human baseline\nuncheckpointed agent edit\n",
    )
    .unwrap();
    insert_session_event(
        &metrics_db_path,
        file_mtime_secs(&file_path),
        "missed-post-edit-session",
        "missed-post-edit-tool-use",
        None,
    );

    let commit = repo
        .stage_all_and_commit("Missed agent checkpoint")
        .expect("mixed-signal commit should succeed");

    let mut file = repo.filename("inherited-human.txt");
    file.assert_committed_lines(lines![
        "known human baseline".human(),
        "uncheckpointed agent edit".unattributed_human(),
    ]);
    assert_human_attests_lines(&commit.authorship_log, "inherited-human.txt", &[1]);
}

#[test]
fn test_known_human_recovery_does_not_inherit_another_contributors_identity() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["config", "user.name", "Alice"]).unwrap();
    repo.git(&["config", "user.email", "alice@example.com"])
        .unwrap();
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("shared.txt");
    fs::write(&file_path, "written by Alice\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .expect("Alice's known-human checkpoint should succeed");
    repo.stage_all_and_commit("Alice baseline")
        .expect("Alice's commit should succeed");

    repo.git(&["config", "user.name", "Bob"]).unwrap();
    repo.git(&["config", "user.email", "bob@example.com"])
        .unwrap();
    fs::write(&file_path, "written by Alice\nuncheckpointed by Bob\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "shared.txt"])
        .expect("Bob's untracked checkpoint should succeed");
    fs::write(repo.path().join("ai-side.txt"), "AI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai-side.txt"])
        .expect("AI checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Bob and AI")
        .expect("Bob's commit should succeed");

    let mut shared = repo.filename("shared.txt");
    shared.assert_committed_lines(lines![
        "written by Alice".human(),
        "uncheckpointed by Bob".unattributed_human(),
    ]);
    let mut ai = repo.filename("ai-side.txt");
    ai.assert_committed_lines(lines!["AI line".ai()]);
    assert_human_attests_lines(&commit.authorship_log, "shared.txt", &[]);
}

#[test]
fn test_known_human_recovery_does_not_cover_untracked_unrelated_files() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let human_path = repo.path().join("human-side.txt");
    fs::write(&human_path, "known human\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "human-side.txt"])
        .expect("known-human checkpoint should succeed");

    fs::write(&human_path, "known human\nlegacy untracked\n").unwrap();
    repo.git_ai(&["checkpoint", "human", "human-side.txt"])
        .expect("legacy human checkpoint should succeed");

    fs::write(
        repo.path().join("unrelated.txt"),
        "uncheckpointed elsewhere\n",
    )
    .unwrap();
    fs::write(repo.path().join("ai-side.txt"), "ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai-side.txt"])
        .expect("AI checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Known human plus unrelated untracked")
        .expect("commit should succeed");

    let mut human_file = repo.filename("human-side.txt");
    human_file.assert_committed_lines(lines![
        "known human".human(),
        "legacy untracked".unattributed_human(),
    ]);
    let mut unrelated_file = repo.filename("unrelated.txt");
    unrelated_file.assert_committed_lines(lines!["uncheckpointed elsewhere".unattributed_human()]);
    let mut ai_file = repo.filename("ai-side.txt");
    ai_file.assert_committed_lines(lines!["ai line".ai()]);
    assert_human_attests_lines(&commit.authorship_log, "human-side.txt", &[1]);
    assert_human_attests_lines(&commit.authorship_log, "unrelated.txt", &[]);
}

#[test]
fn test_pure_human_recovery_skips_unknown_lines_when_commit_contains_ai() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let env = [
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str()),
        ("GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH", bash_db_path.as_str()),
    ];
    let repo = TestRepo::new_with_daemon_env(&env);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    fs::write(repo.path().join("manual.txt"), "manual line\n").unwrap();
    fs::write(repo.path().join("ai.txt"), "ai line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "ai.txt"])
        .expect("AI checkpoint should succeed");

    let commit = repo
        .stage_all_and_commit("Manual plus AI")
        .expect("commit should succeed");

    let mut manual = repo.filename("manual.txt");
    manual.assert_committed_lines(lines!["manual line".unattributed_human()]);
    let mut ai = repo.filename("ai.txt");
    ai.assert_committed_lines(lines!["ai line".ai()]);
    assert_human_attests_lines(&commit.authorship_log, "manual.txt", &[]);
}

#[test]
fn test_session_event_recovery_ignores_events_outside_window() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("outside.txt");
    fs::write(&file_path, "outside the window\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_ts.saturating_sub(10),
        "external-outside-window-session",
        "tool-use-outside-window",
        None,
    );

    let commit = repo
        .stage_all_and_commit("Outside window stays unknown")
        .expect("commit should succeed");

    let mut file = repo.filename("outside.txt");
    file.assert_committed_lines(lines!["outside the window".unattributed_human()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "outside-window session event must not recover attribution"
    );
}

#[test]
fn test_session_event_recovery_rejects_time_only_sessions() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("time-only.txt");
    fs::write(&file_path, "time only\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let recovered_session_id = insert_session_event(
        &metrics_db_path,
        file_ts,
        "external-time-only",
        "tool-use-time-only",
        None,
    );

    let commit = repo
        .stage_all_and_commit("Time-only session stays unknown")
        .expect("commit should succeed");

    let mut file = repo.filename("time-only.txt");
    file.assert_committed_lines(lines!["time only".unattributed_human()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&recovered_session_id),
        "time-only session events must not recover attribution"
    );
}

#[test]
fn test_commit_metadata_recovery_uses_existing_matching_session_after_edge_expansion() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });
    let file_path = repo.path().join("metadata-existing.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial base")
        .expect("initial commit should succeed");
    let mut file = repo.filename("metadata-existing.txt");
    file.assert_committed_lines(lines!["base".unattributed_human()]);

    let external_session_id = "codex-existing-metadata-session";
    codex_checkpoint(
        &repo,
        &file_path,
        external_session_id,
        "PreToolUse",
        "metadata-existing-tool",
    );

    fs::write(&file_path, "base\ncodex line\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        external_session_id,
        "PostToolUse",
        "metadata-existing-tool",
    );

    fs::write(
        &file_path,
        "\
base
codex line
unknown 1
unknown 2
unknown 3
unknown 4
unknown 5
unknown 6
",
    )
    .unwrap();

    let commit = repo
        .stage_all_and_commit(
            "Recover from Codex trailer\n\nCo-authored-by: Codex <noreply@openai.com>",
        )
        .expect("commit should succeed");

    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "codex line".ai(),
        "unknown 1".ai(),
        "unknown 2".ai(),
        "unknown 3".ai(),
        "unknown 4".ai(),
        "unknown 5".ai(),
        "unknown 6".ai(),
    ]);
    let expected_session_id = generate_session_id(external_session_id, "codex");
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-existing.txt",
        &expected_session_id,
        &[2, 3, 4, 5, 6, 7, 8],
    );
}

#[test]
fn test_commit_metadata_recovery_skips_when_edge_expansion_recovers_all_unknown_lines() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.exclude_prompts_in_repositories = Some(vec![]);
    });
    let file_path = repo.path().join("metadata-edge-skip.txt");

    fs::write(&file_path, "base\n").unwrap();
    repo.stage_all_and_commit("Initial base")
        .expect("initial commit should succeed");
    let mut file = repo.filename("metadata-edge-skip.txt");
    file.assert_committed_lines(lines!["base".unattributed_human()]);

    let external_session_id = "codex-edge-skip-session";
    codex_checkpoint(
        &repo,
        &file_path,
        external_session_id,
        "PreToolUse",
        "metadata-edge-skip-tool",
    );

    fs::write(&file_path, "base\ncodex line\n").unwrap();
    codex_checkpoint(
        &repo,
        &file_path,
        external_session_id,
        "PostToolUse",
        "metadata-edge-skip-tool",
    );

    fs::write(
        &file_path,
        "\
base
codex line
edge 1
edge 2
edge 3
",
    )
    .unwrap();

    let commit = repo
        .stage_all_and_commit(
            "Edge should win before metadata\n\nCo-authored-by: Claude <noreply@anthropic.com>",
        )
        .expect("commit should succeed");

    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "codex line".ai(),
        "edge 1".ai(),
        "edge 2".ai(),
        "edge 3".ai(),
    ]);
    let expected_session_id = generate_session_id(external_session_id, "codex");
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-edge-skip.txt",
        &expected_session_id,
        &[2, 3, 4, 5],
    );
    assert!(
        session_ids_for_tool(&commit.authorship_log, "claude").is_empty(),
        "commit metadata should not synthesize a Claude session after edge recovery removes all unknown lines"
    );
}

#[test]
fn test_commit_metadata_recovery_uses_nearest_matching_tool_session_event() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("metadata-nearest.txt");
    fs::write(&file_path, "cursor recovered from trailer\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let far_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts.saturating_sub(2),
        "cursor",
        "cursor-far-session",
        "cursor-far-tool",
        None,
    );
    let near_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts,
        "cursor",
        "cursor-near-session",
        "cursor-near-tool",
        None,
    );

    let commit = repo
        .stage_all_and_commit(
            "Recover from Cursor trailer\n\nCo-authored-by: Cursor <cursoragent@cursor.com>",
        )
        .expect("commit should succeed");

    let mut file = repo.filename("metadata-nearest.txt");
    file.assert_committed_lines(lines!["cursor recovered from trailer".ai()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&far_session_id),
        "nearest timestamp recovery should not select the farther Cursor session"
    );
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-nearest.txt",
        &near_session_id,
        &[1],
    );
}

#[test]
fn test_commit_metadata_recovery_falls_back_to_most_recent_matching_tool_session() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("metadata-latest.txt");
    fs::write(&file_path, "claude recovered from trailer\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let older_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts.saturating_sub(200),
        "claude",
        "claude-older-session",
        "claude-older-tool",
        None,
    );
    let latest_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts.saturating_sub(100),
        "claude",
        "claude-latest-session",
        "claude-latest-tool",
        None,
    );

    let commit = repo
        .stage_all_and_commit(
            "Recover from Claude trailer\n\nCo-authored-by: Claude Opus 4.8 <noreply@anthropic.com>",
        )
        .expect("commit should succeed");

    let mut file = repo.filename("metadata-latest.txt");
    file.assert_committed_lines(lines!["claude recovered from trailer".ai()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&older_session_id),
        "latest fallback should not select an older Claude session"
    );
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-latest.txt",
        &latest_session_id,
        &[1],
    );
}

#[test]
fn test_commit_metadata_recovery_latest_matching_tool_prefers_current_repo_url() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&[
        "remote",
        "add",
        "origin",
        "https://github.com/acme/metadata-repo-url.git",
    ])
    .expect("remote add should succeed");
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("metadata-latest-repo-url.txt");
    fs::write(&file_path, "cursor recovered with repo url\n").unwrap();
    let file_ts = file_mtime_secs(&file_path);
    let other_repo_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts.saturating_sub(100),
        "cursor",
        "cursor-other-repo-session",
        "cursor-other-repo-tool",
        Some("https://github.com/acme/other-repo"),
    );
    let same_repo_session_id = insert_session_event_for_tool(
        &metrics_db_path,
        file_ts.saturating_sub(200),
        "cursor",
        "cursor-same-repo-session",
        "cursor-same-repo-tool",
        Some("https://github.com/acme/metadata-repo-url"),
    );

    let commit = repo
        .stage_all_and_commit(
            "Recover from Cursor trailer\n\nCo-authored-by: Cursor <cursoragent@cursor.com>",
        )
        .expect("commit should succeed");

    let mut file = repo.filename("metadata-latest-repo-url.txt");
    file.assert_committed_lines(lines!["cursor recovered with repo url".ai()]);
    assert!(
        !commit
            .authorship_log
            .metadata
            .sessions
            .contains_key(&other_repo_session_id),
        "latest fallback should reject an explicit session event from another repo"
    );
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-latest-repo-url.txt",
        &same_repo_session_id,
        &[1],
    );
}

#[test]
fn test_commit_metadata_recovery_synthesizes_session_from_agent_author_email() {
    let (_metrics_db_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo =
        TestRepo::new_with_daemon_env(&[("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path.as_str())]);
    repo.git(&["commit", "--allow-empty", "-m", "initial"])
        .expect("initial empty commit should succeed");

    let file_path = repo.path().join("metadata-author-email.txt");
    fs::write(&file_path, "codex author email fallback\n").unwrap();

    let commit = repo
        .stage_all_and_commit_with_env(
            "Recover from author email",
            &[
                ("GIT_AUTHOR_NAME", "Codex"),
                ("GIT_AUTHOR_EMAIL", "codex@openai.com"),
            ],
        )
        .expect("commit should succeed");

    let mut file = repo.filename("metadata-author-email.txt");
    file.assert_committed_lines(lines!["codex author email fallback".ai()]);

    let codex_sessions = session_ids_for_tool(&commit.authorship_log, "codex");
    assert_eq!(
        codex_sessions.len(),
        1,
        "author-email fallback should synthesize one Codex session"
    );
    assert!(
        codex_sessions[0].starts_with("s_"),
        "synthesized session ids should use the session id namespace"
    );
    assert_session_attests_lines(
        &commit.authorship_log,
        "metadata-author-email.txt",
        &codex_sessions[0],
        &[1],
    );
}

#[test]
fn test_commit_metadata_recovery_ignores_freeform_message_agent_mentions() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("metadata-freeform-agent-mention.txt");
    fs::write(&file_path, "freeform codex mention should stay unknown\n").unwrap();

    let commit = repo
        .stage_all_and_commit_with_env(
            "codex did things",
            &[
                ("GIT_AUTHOR_NAME", "Sasha Varlamov"),
                ("GIT_AUTHOR_EMAIL", "sasha@sashavarlamov.com"),
            ],
        )
        .expect("freeform mention commit should succeed");

    let mut file = repo.filename("metadata-freeform-agent-mention.txt");
    file.assert_committed_lines(lines![
        "freeform codex mention should stay unknown".unattributed_human()
    ]);
    assert!(
        commit.authorship_log.metadata.sessions.is_empty(),
        "freeform message text mentioning Codex should not synthesize an AI session"
    );
}

#[test]
fn test_commit_metadata_recovery_ignores_ambiguous_identity_markers() {
    let repo = TestRepo::new();

    let amp_file_path = repo.path().join("metadata-ambiguous-amp.txt");
    fs::write(&amp_file_path, "ambiguous amp trailer\n").unwrap();
    let amp_commit = repo
        .stage_all_and_commit("Ambiguous AMP trailer\n\nCo-authored-by: AMP Team <team@amp.dev>")
        .expect("amp trailer commit should succeed");

    let mut amp_file = repo.filename("metadata-ambiguous-amp.txt");
    amp_file.assert_committed_lines(lines!["ambiguous amp trailer".unattributed_human()]);
    assert!(
        amp_commit.authorship_log.metadata.sessions.is_empty(),
        "ambiguous AMP identity should not synthesize an AI session"
    );

    let continue_file_path = repo.path().join("metadata-ambiguous-continue.txt");
    fs::write(&continue_file_path, "ambiguous continue trailer\n").unwrap();
    let continue_commit = repo
        .stage_all_and_commit(
            "Ambiguous Continue trailer\n\nCo-authored-by: Continue Project <dev@continue.dev>",
        )
        .expect("continue trailer commit should succeed");

    let mut continue_file = repo.filename("metadata-ambiguous-continue.txt");
    continue_file.assert_committed_lines(lines!["ambiguous continue trailer".unattributed_human()]);
    assert!(
        continue_commit.authorship_log.metadata.sessions.is_empty(),
        "ambiguous Continue identity should not synthesize an AI session"
    );

    let openai_file_path = repo.path().join("metadata-ambiguous-openai.txt");
    fs::write(&openai_file_path, "generic openai noreply trailer\n").unwrap();
    let openai_commit = repo
        .stage_all_and_commit(
            "Ambiguous OpenAI trailer\n\nCo-authored-by: OpenAI Release Bot <noreply@openai.com>",
        )
        .expect("generic OpenAI noreply trailer commit should succeed");

    let mut openai_file = repo.filename("metadata-ambiguous-openai.txt");
    openai_file.assert_committed_lines(lines![
        "generic openai noreply trailer".unattributed_human()
    ]);
    assert!(
        openai_commit.authorship_log.metadata.sessions.is_empty(),
        "generic OpenAI noreply identity should not synthesize a Codex session"
    );

    for (file_name, line, identity, message) in [
        (
            "metadata-ambiguous-claude.txt",
            "human named claude trailer",
            "Claude Smith <claude.smith@example.com>",
            "ambiguous Claude human identity should not synthesize an AI session",
        ),
        (
            "metadata-ambiguous-devin.txt",
            "human named devin trailer",
            "Devin Patel <devin.patel@example.com>",
            "ambiguous Devin human identity should not synthesize an AI session",
        ),
        (
            "metadata-ambiguous-gemini.txt",
            "human named gemini trailer",
            "Gemini Jones <gemini.jones@example.com>",
            "ambiguous Gemini human identity should not synthesize an AI session",
        ),
    ] {
        let path = repo.path().join(file_name);
        fs::write(&path, format!("{line}\n")).unwrap();
        let commit = repo
            .stage_all_and_commit(&format!(
                "Ambiguous human trailer\n\nCo-authored-by: {identity}"
            ))
            .expect("ambiguous human identity commit should succeed");

        let mut file = repo.filename(file_name);
        file.assert_committed_lines(lines![line.unattributed_human()]);
        assert!(
            commit.authorship_log.metadata.sessions.is_empty(),
            "{message}"
        );
    }
}
