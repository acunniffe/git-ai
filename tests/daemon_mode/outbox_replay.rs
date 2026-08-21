#![cfg(any(target_os = "linux", target_os = "macos"))]
//! Checkpoint-outbox replay: records published while no daemon was reachable
//! must be applied by the next daemon, exactly once, with eligibility
//! rechecked at replay time.

use super::*;
use git_ai::model::checkpoint_delivery::CheckpointDelivery;
use git_ai::model::repository::checkpoint_outbox::publish_delivery;
use git_ai::model::working_log::AgentId;
use std::collections::HashMap;
use std::path::PathBuf;

const FAST_POLL_ENV: (&str, &str) = ("GIT_AI_TEST_OUTBOX_POLL_MS", "100");

fn publish_checkpoint_while_daemon_down(repo: &TestRepo, file_rel: &str, ai_line: &str) -> PathBuf {
    let file_path = repo.path().join(file_rel);
    fs::write(&file_path, "base\n").expect("failed to write base fixture");
    repo.git_og(&["add", file_rel])
        .expect("failed to stage base fixture");
    repo.git_og(&["commit", "-m", "base commit"])
        .expect("failed to create base commit");

    fs::write(&file_path, format!("base\n{ai_line}\n")).expect("failed to write AI edit");
    let output = repo
        .git_ai_command_without_pre_sync_for_test(&["checkpoint", "mock_ai", file_rel], &[])
        .output()
        .expect("failed to invoke checkpoint without a daemon");
    assert!(
        output.status.success(),
        "checkpoint hooks must keep their exit-zero contract: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let records = ready_checkpoint_outbox_records(repo);
    assert_eq!(
        records.len(),
        1,
        "daemon-less checkpoint must publish exactly one ready record: {records:?}"
    );
    records.into_iter().next().expect("record path")
}

fn wait_for_outbox_drained(repo: &TestRepo) {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if ready_checkpoint_outbox_records(repo).is_empty() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "daemon did not replay the ready outbox record in time: {:?}",
            ready_checkpoint_outbox_records(repo)
        );
        thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn outbox_record_published_while_daemon_down_is_replayed_into_attribution() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    publish_checkpoint_while_daemon_down(&repo, "replayed.txt", "captured while daemon was down");

    let daemon = DaemonGuard::start_with_env(&repo, &[FAST_POLL_ENV]);
    wait_for_outbox_drained(&repo);

    // The replayed checkpoint sits in the working log; a traced commit must
    // now attribute the AI line exactly as if the delivery had been live.
    let env = git_trace_env(&daemon.trace_socket_path);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let baseline = repo.daemon_total_completion_count();
    let mut expected_completions = 0u64;
    traced_git_with_env(
        &repo,
        &["add", "replayed.txt"],
        &env_refs,
        &mut expected_completions,
    )
    .expect("traced add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit replayed checkpoint"],
        &env_refs,
        &mut expected_completions,
    )
    .expect("traced commit should succeed");
    wait_for_expected_top_level_completions(&repo, baseline, expected_completions);

    assert_blame_lines_for_workdir(
        &repo,
        repo.path(),
        "replayed.txt",
        &[
            ("base".to_string(), false),
            ("captured while daemon was down".to_string(), true),
        ],
    );
}

#[test]
fn outbox_replay_deduplicates_an_already_applied_delivery() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let record_path =
        publish_checkpoint_while_daemon_down(&repo, "deduped.txt", "applied exactly once");
    let root = record_path
        .parent()
        .expect("ready record has a root directory")
        .to_path_buf();
    let delivery = git_ai::model::repository::checkpoint_outbox::decode_delivery(
        &fs::read(&record_path).expect("failed to read ready record"),
    )
    .expect("published record should decode");

    let _daemon = DaemonGuard::start_with_env(&repo, &[FAST_POLL_ENV]);
    wait_for_outbox_drained(&repo);

    let checkpoints_after_first_replay = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("working log should read");
    let applied_ids: Vec<_> = checkpoints_after_first_replay
        .iter()
        .filter(|checkpoint| checkpoint.delivery_id.as_deref() == Some(&delivery.delivery_id))
        .collect();
    assert_eq!(
        applied_ids.len(),
        1,
        "first replay must record the delivery id exactly once"
    );

    // Re-publish the very same delivery (at-least-once redelivery) and let
    // the daemon consume it again: dedup must keep the working log unchanged.
    publish_delivery(&root, &delivery).expect("re-publishing the same delivery should succeed");
    wait_for_outbox_drained(&repo);

    let checkpoints_after_replay = repo
        .current_working_logs()
        .read_all_checkpoints()
        .expect("working log should read");
    assert_eq!(
        checkpoints_after_replay.len(),
        checkpoints_after_first_replay.len(),
        "replaying an already-applied delivery must not append checkpoints"
    );
    assert_eq!(
        checkpoints_after_replay
            .iter()
            .filter(|checkpoint| checkpoint.delivery_id.as_deref() == Some(&delivery.delivery_id))
            .count(),
        1,
        "the delivery id must stay recorded exactly once"
    );
}

#[test]
fn outbox_replay_quarantines_record_for_disallowed_repository() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    // Restrict collection to exactly this repository; the record below targets
    // a different repository and must be discarded at replay time.
    let repo_root = repo.canonical_path().to_string_lossy().replace('\\', "/");
    repo.patch_git_ai_config(move |patch| {
        patch.allowed_repositories = Some(vec![repo_root]);
    });

    let other = tempfile::tempdir().expect("failed to create disallowed repo dir");
    let other_repo = other.path().join("disallowed-repo");
    fs::create_dir_all(&other_repo).expect("failed to create disallowed repo");
    RawGitCommand::in_working_dir(&other_repo, &["init"])
        .configure(|command| configure_test_home_env(command, repo.test_home_path()))
        .output()
        .expect("git init should run");
    let other_file = other_repo.join("ineligible.txt");
    fs::write(&other_file, "ai content\n").expect("failed to write disallowed fixture");

    let request = CheckpointRequest {
        trace_id: "outbox-replay-ineligible".to_string(),
        checkpoint_kind: CheckpointKind::AiAgent,
        agent_id: Some(AgentId {
            tool: "mock_ai".to_string(),
            id: "ai-thread-ineligible".to_string(),
            model: "unknown".to_string(),
        }),
        files: vec![CheckpointFile {
            path: other_file.clone(),
            content: Some("ai content\n".to_string()),
            repo_work_dir: other_repo.clone(),
            base_commit: BaseCommit::Initial,
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
        delivery_id: None,
    };
    let delivery = CheckpointDelivery::from_requests(vec![request])
        .into_iter()
        .next()
        .expect("one delivery");

    let daemon_config = DaemonConfig::from_home(&repo.daemon_home_path());
    let root = candidate_roots(
        &daemon_config.internal_dir,
        None,
        &std::env::temp_dir(),
        unsafe { libc::geteuid() },
    )
    .expect("candidate roots")
    .into_iter()
    .next()
    .expect("at least one candidate root");
    publish_delivery(&root, &delivery).expect("publishing the disallowed record should succeed");

    let _daemon = DaemonGuard::start_with_env(&repo, &[FAST_POLL_ENV]);
    wait_for_outbox_drained(&repo);

    let quarantined: Vec<_> = fs::read_dir(&root)
        .expect("outbox root should read")
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.extension()
                .and_then(|extension| extension.to_str())
                .is_some_and(|extension| extension == "quarantined")
        })
        .collect();
    assert_eq!(
        quarantined.len(),
        1,
        "the disallowed record must be quarantined, not applied or retried forever"
    );
}
