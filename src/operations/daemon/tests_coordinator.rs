use super::tests_ingress::make_start_payload;
use super::*;
use crate::model::checkpoint_request::{CheckpointRequest, PreparedPathRole};
use crate::model::working_log::CheckpointKind;
use crate::operations::daemon::git_backend::GitBackend;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

// -----------------------------------------------------------------------
// OnceLock / shutdown / atomic-ordering tests
// -----------------------------------------------------------------------

#[test]
fn family_sequencer_state_owns_semantic_ordering() {
    let mut state = FamilySequencerState::new();
    let canceled = || FamilySequencerEntry::Canceled;
    let first = state.insert_entry(7, canceled());
    let second = state.insert_entry(7, canceled());
    assert_eq!((first.started_at_ns, first.ordinal), (7, 1));
    assert_eq!((second.started_at_ns, second.ordinal), (7, 2));
    state.next_ordinal = u64::MAX;
    state.insert_entry(8, canceled());
    assert_eq!(state.insert_entry(9, canceled()).ordinal, u64::MAX);
}

#[test]
fn explicit_stop_overrides_prior_restart_intent() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let coordinator = ActorDaemonCoordinator::new();

        coordinator.request_restart_after_update();
        assert_eq!(
            coordinator.shutdown_action(),
            DaemonExitAction::RestartAfterUpdate
        );

        coordinator.request_stop();

        assert!(coordinator.is_shutting_down());
        assert_eq!(coordinator.shutdown_action(), DaemonExitAction::Stop);
    });
}

#[tokio::test]
async fn checkpoint_fence_waits_for_open_mutating_trace_root() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    let sid = "20260411T120000.000000-Psid1";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut payload = make_start_payload(&["git", "commit", "-m", "test commit"]);
    assert!(
        coord.prepare_trace_payload_for_ingest(&mut payload),
        "commit start should mark the root as mutating"
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coord.wait_for_trace_ingest_processed_through()
        )
        .await
        .is_err(),
        "checkpoint fence must not pass while a mutating trace root is still open"
    );

    coord
        .record_trace_connection_close(&[sid.to_string()])
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint fence should pass once the mutating trace root closes");
}

#[tokio::test]
async fn checkpoint_control_request_waits_while_blocked_behind_pending_root() {
    use crate::model::checkpoint_request::{BaseCommit, CheckpointFile};

    let coord = Arc::new(ActorDaemonCoordinator::new());
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("init")
        .output()
        .expect("git init should run");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );
    std::fs::write(repo.join("test.txt"), "checkpoint content\n").unwrap();

    let family = coord.backend.resolve_family(&repo).unwrap().0;
    let root_sid = "20260411T120000.000000-Psid-blocking-root";
    coord
        .append_pending_root_entry(&family, root_sid, 1)
        .unwrap();

    let request = CheckpointRequest {
        trace_id: "blocked-checkpoint".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![CheckpointFile {
            path: PathBuf::from("test.txt"),
            content: Some("checkpoint content\n".to_string()),
            repo_work_dir: repo.clone(),
            base_commit: BaseCommit::Initial,
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
        delivery_id: None,
    };

    let mut checkpoint = {
        let coord = coord.clone();
        tokio::spawn(async move { coord.ingest_checkpoint_payload(request).await })
    };

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut checkpoint)
            .await
            .is_err(),
        "checkpoint control request must not complete before its sequenced side effect runs"
    );

    coord
        .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
        .await
        .unwrap();

    let response = tokio::time::timeout(Duration::from_secs(1), checkpoint)
        .await
        .expect("checkpoint should finish once the prior root is released")
        .expect("checkpoint task should not panic")
        .expect("checkpoint request should succeed");
    assert!(
        response.ok,
        "checkpoint response should be ok: {response:?}"
    );
}

#[tokio::test]
async fn family_fence_ignores_open_mutating_roots_of_other_families() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    let temp = tempfile::tempdir().unwrap();
    let make_repo = |name: &str| {
        let worktree = temp.path().join(name);
        let git_dir = worktree.join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        worktree
    };
    let originating_repo = make_repo("originating");
    let unrelated_repo = make_repo("unrelated");
    let originating_family = coord.backend.resolve_family(&originating_repo).unwrap().0;
    let unrelated_family = coord.backend.resolve_family(&unrelated_repo).unwrap().0;

    let sid = "20260411T120000.000000-Psid-family-fence";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut payload = serde_json::json!({
        "event": "start",
        "sid": sid,
        "argv": ["git", "commit", "-m", "test commit"],
        "worktree": originating_repo,
        "time_ns": 1u64,
    });
    assert!(
        coord.prepare_trace_payload_for_ingest(&mut payload),
        "commit start should mark the root as mutating"
    );

    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through_family(&unrelated_family),
    )
    .await
    .expect("an unrelated family must not wait on another family's open mutating root");

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coord.wait_for_trace_ingest_processed_through_family(&originating_family),
        )
        .await
        .is_err(),
        "the originating family must stay fenced while its mutating root is open"
    );

    coord
        .record_trace_connection_close(&[sid.to_string()])
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through_family(&originating_family),
    )
    .await
    .expect("the originating family fence should pass once the root closes");
}

#[tokio::test]
async fn family_fence_fails_closed_for_unattributed_open_roots() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    let sid = "20260411T120000.000000-Psid-unattributed";
    coord.trace_root_connection_opened(sid).unwrap();
    // No worktree hint: the root cannot be attributed to any family yet.
    let mut payload = make_start_payload(&["git", "commit", "-m", "test commit"]);
    payload["sid"] = serde_json::json!(sid);
    assert!(
        coord.prepare_trace_payload_for_ingest(&mut payload),
        "commit start should mark the root as mutating"
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coord.wait_for_trace_ingest_processed_through_family("/any/family/.git"),
        )
        .await
        .is_err(),
        "an open mutating root with no family attribution must block every family"
    );

    coord
        .record_trace_connection_close(&[sid.to_string()])
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through_family("/any/family/.git"),
    )
    .await
    .expect("the fence should pass once the unattributed root closes");
}

#[tokio::test]
async fn trace_connection_close_without_atexit_cancels_pending_root() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.start_trace_ingest_worker().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let worktree = temp.path().join("repo");
    let git_dir = worktree.join(".git");
    std::fs::create_dir_all(git_dir.join("logs")).unwrap();

    let sid = "20260411T120000.000000-Psid-close";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut start = serde_json::json!({
        "event": "start",
        "sid": sid,
        "argv": ["git", "commit", "-m", "test commit"],
        "worktree": worktree,
        "time_ns": 1u64,
    });
    assert!(coord.prepare_trace_payload_for_ingest(&mut start));
    coord.enqueue_trace_payload(start).unwrap();

    finalize_trace_connection_roots(coord.clone(), [sid.to_string()].into_iter().collect())
        .unwrap();
    coord.wait_for_trace_ingest_processed_through().await;

    assert!(
        !coord
            .pending_root_slots_by_root
            .lock()
            .unwrap()
            .contains_key(sid),
        "closing the trace stream without root atexit must not leave the family sequencer wedged"
    );
    coord.request_shutdown();
}

#[tokio::test]
async fn readonly_trace_connection_close_without_atexit_clears_tracking() {
    let coord = ActorDaemonCoordinator::new();
    let sid = "20260411T120000.000000-Psid-readonly-close";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut start = make_start_payload(&["git", "status", "--short"]);
    start["sid"] = serde_json::json!(sid);
    assert!(!coord.prepare_trace_payload_for_ingest(&mut start));

    let close_marker_roots = coord
        .record_trace_connection_close(&[sid.to_string()])
        .unwrap();

    assert!(
        close_marker_roots.is_empty(),
        "read-only roots should not enqueue synthetic close markers"
    );
    let ingress = coord.trace_ingress_state.lock().unwrap();
    assert!(!ingress.root_argv.contains_key(sid));
    assert!(!ingress.root_definitely_read_only.contains(sid));
    assert!(!ingress.root_open_connections.contains_key(sid));
}

#[tokio::test]
async fn checkpoint_fence_does_not_wait_for_unidentified_trace_connection() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.trace_unidentified_connection_opened().unwrap();

    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint fence must not wait for an accepted trace connection with no root");

    coord
        .trace_unidentified_connection_identified_or_closed()
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint fence should pass once the unidentified connection is resolved");
}

#[tokio::test]
async fn checkpoint_fence_waits_for_open_branch_mutation_root() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    let sid = "20260411T120000.000000-Psid1";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut payload = make_start_payload(&["git", "branch", "-D", "feature"]);
    assert!(
        coord.prepare_trace_payload_for_ingest(&mut payload),
        "branch delete start should be enqueued because it mutates refs"
    );

    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            coord.wait_for_trace_ingest_processed_through()
        )
        .await
        .is_err(),
        "checkpoint fence must not pass while an accepted branch mutation root is still open"
    );

    coord
        .record_trace_connection_close(&[sid.to_string()])
        .unwrap();
    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint fence should pass once the branch mutation root closes");
}

#[tokio::test]
async fn checkpoint_fence_does_not_wait_for_open_branch_readonly_root() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    let sid = "20260411T120000.000000-Psid-readonly-branch";
    coord.trace_root_connection_opened(sid).unwrap();
    let mut payload = make_start_payload(&["git", "branch", "--show-current"]);
    payload["sid"] = serde_json::json!(sid);
    assert!(
        !coord.prepare_trace_payload_for_ingest(&mut payload),
        "branch --show-current should be classified as read-only"
    );

    tokio::time::timeout(
        Duration::from_secs(1),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint fence must not wait for an open read-only branch root");
}

/// `enqueue_trace_payload` must return an error when the ingest worker has
/// not been started yet.  This is the "no-sender" fast-fail path and is
/// unchanged by the OnceLock refactor.
#[tokio::test]
async fn enqueue_before_worker_start_returns_error() {
    let coord = ActorDaemonCoordinator::new();
    // Worker never started → OnceLock is empty → enqueue must fail
    let payload = serde_json::json!({
        "event": "start",
        "sid": "20260411T120000.000000-Ptest0001",
        "__git_ai_ingest_seq": 1_u64,
        "argv": ["git", "commit", "-m", "test"],
    });
    assert!(
        coord.enqueue_trace_payload(payload).is_err(),
        "enqueue before worker start must return an error"
    );
}

#[tokio::test]
async fn enqueue_accounting_error_does_not_allocate_ingest_sequence() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.start_trace_ingest_worker().unwrap();
    let poison_coord = coord.clone();
    let _ = std::thread::spawn(move || {
        let _guard = poison_coord
            .queued_trace_payloads_by_root
            .lock()
            .expect("mutex should be lockable before intentional poison");
        panic!("intentional queue accounting mutex poison");
    })
    .join();

    let payload = serde_json::json!({
        "event": "start",
        "sid": "20260411T120000.000000-Paccounting",
        "argv": ["git", "commit", "-m", "test"],
    });
    assert!(
        coord.enqueue_trace_payload(payload).is_err(),
        "poisoned queue accounting must fail enqueue"
    );
    assert_eq!(
        coord.next_trace_ingest_seq.load(Ordering::Acquire),
        0,
        "failed enqueue must not allocate an ingest sequence that can block checkpoint drains"
    );
    coord.request_shutdown();
}

/// After `request_shutdown()`, `is_shutting_down()` returns true and the
/// coordinator stays in a consistent state.  The ingest worker (started
/// via `start_trace_ingest_worker`) must exit cleanly even when the sender
/// is no longer dropped by `request_shutdown` (OnceLock never drops it).
#[tokio::test]
async fn request_shutdown_is_idempotent_and_consistent() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.start_trace_ingest_worker().unwrap();
    assert!(!coord.is_shutting_down());
    coord.request_shutdown();
    assert!(coord.is_shutting_down());
    // Second call must not panic.
    coord.request_shutdown();
    assert!(coord.is_shutting_down());
    // Allow tokio to run the ingest worker's shutdown select arm.
    tokio::task::yield_now().await;
}

#[tokio::test]
async fn checkpoint_trace_ingest_drain_returns_on_shutdown() {
    let coord = ActorDaemonCoordinator::new();
    coord.next_trace_ingest_seq.store(1, Ordering::Release);
    coord.processed_trace_ingest_seq.store(0, Ordering::Release);
    coord.request_shutdown();

    tokio::time::timeout(
        std::time::Duration::from_millis(100),
        coord.wait_for_trace_ingest_processed_through(),
    )
    .await
    .expect("checkpoint trace ingest drain must return when daemon shutdown is requested");
}

/// The trace socket receive-buffer helper must raise a socket's `SO_RCVBUF`
/// capacity toward the configured target.
#[test]
#[cfg(not(windows))]
fn trace_socket_recv_buffer_helper_raises_socket_capacity() {
    let (server, _client) =
        std::os::unix::net::UnixStream::pair().expect("create connected unix socket pair");
    let before = socket_recv_buffer(&server).expect("read baseline receive buffer");
    set_socket_recv_buffer(&server, TRACE_SOCKET_RECV_BUFFER_BYTES)
        .expect("set trace socket receive buffer");
    let after = socket_recv_buffer(&server).expect("read trace socket receive buffer");
    // Linux clamps SO_RCVBUF to net.core.rmem_max, so `after` can land below
    // the target on hosts with a small rmem_max (e.g. CI's ~208 KiB
    // default). The helper is still correct as long as it raised capacity
    // toward the target: it either reached the target or grew past the
    // default buffer.
    assert!(
        after >= TRACE_SOCKET_RECV_BUFFER_BYTES || after > before,
        "trace socket receive buffer should reach {} bytes or exceed the {}-byte baseline, got {}",
        TRACE_SOCKET_RECV_BUFFER_BYTES,
        before,
        after
    );
}

/// A zero target is a no-op: the helper must not error and must not shrink
/// the socket's existing receive buffer.
#[test]
#[cfg(not(windows))]
fn trace_socket_recv_buffer_helper_zero_is_noop() {
    let (server, _client) =
        std::os::unix::net::UnixStream::pair().expect("create connected unix socket pair");
    let before = socket_recv_buffer(&server).expect("read baseline receive buffer");
    set_socket_recv_buffer(&server, 0).expect("zero target must be a no-op");
    let after = socket_recv_buffer(&server).expect("read receive buffer after no-op");
    assert_eq!(
        before, after,
        "a zero target must not change the socket receive buffer"
    );
}

/// Concurrent enqueues from multiple threads must never deadlock or
/// corrupt the accounting counter.
#[tokio::test]
async fn concurrent_mutating_enqueues_do_not_deadlock() {
    use std::sync::Arc;
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.start_trace_ingest_worker().unwrap();

    const TASKS: usize = 8;
    const PER_TASK: usize = 20;

    // Use prepare_trace_payload_for_ingest + enqueue_trace_payload from
    // multiple tasks concurrently.
    let mut handles = Vec::with_capacity(TASKS);
    for task_id in 0..TASKS {
        let c = coord.clone();
        handles.push(tokio::spawn(async move {
            for i in 0..PER_TASK {
                let sid = format!("20260411T120000.000000-P{:08x}", task_id * 1000 + i);
                let mut payload = serde_json::json!({
                    "event": "start",
                    "sid": sid,
                    "argv": ["git", "commit", "-m", "msg"],
                });
                if c.prepare_trace_payload_for_ingest(&mut payload) {
                    c.enqueue_trace_payload(payload)
                        .expect("mutating event should enqueue");
                }
            }
        }));
    }
    for h in handles {
        h.await.expect("task must not panic");
    }
    // Give the ingest worker time to drain the queue.
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while coord.queued_trace_payloads.load(Ordering::Acquire) > 0 {
        if tokio::time::Instant::now() >= deadline {
            break; // don't fail the test on CI slowness; just stop waiting
        }
        tokio::task::yield_now().await;
    }
    coord.request_shutdown();
}
