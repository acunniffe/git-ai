use super::*;
use serde_json::Value;
use std::sync::Arc;
use std::sync::atomic::Ordering;

// -----------------------------------------------------------------------
// Readonly command ingress fast-path tests
//
// These tests verify that prepare_trace_payload_for_ingest returns false
// (do-not-enqueue) for read-only commands and true for mutating ones, and
// that the queued_trace_payloads counter is not incremented for read-only
// events.
//
// ActorDaemonCoordinator::new() spawns Tokio tasks internally, so all
// tests that construct one must run inside a Tokio runtime.
// -----------------------------------------------------------------------

pub(super) fn make_start_payload(argv: &[&str]) -> Value {
    serde_json::json!({
        "event": "start",
        "sid": "20260411T120000.000000-Psid1",
        "argv": argv,
    })
}

pub(super) fn make_atexit_payload(sid: &str) -> Value {
    serde_json::json!({
        "event": "atexit",
        "sid": sid,
        "code": 0,
    })
}

#[test]
fn exit_is_not_a_root_completion_boundary() {
    let sid = "20260411T120000.000000-Psid1";

    assert!(
        !is_terminal_root_trace_event("exit", sid, sid),
        "trace2 exit can fire before Git atexit cleanup and must not complete root processing"
    );
    assert!(is_terminal_root_trace_event("atexit", sid, sid));
}

#[tokio::test]
async fn readonly_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&["git", "status", "--short"]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "status start event should not be enqueued (readonly)"
    );
    assert_eq!(
        coord.queued_trace_payloads.load(Ordering::Relaxed),
        0,
        "queued_trace_payloads should stay 0 for readonly start event"
    );
    // Readonly events must NOT receive an ingest sequence number
    assert!(
        payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
        "readonly start event must not receive an ingest sequence number"
    );
}

#[tokio::test]
async fn stash_list_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "-c",
        "core.fsmonitor=false",
        "--no-pager",
        "stash",
        "list",
        "--pretty=format:%gd%x00%H%x00%ct%x00%s",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "stash list start event should not be enqueued (readonly invocation)"
    );
    assert!(
        payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
        "stash list start event must not receive an ingest sequence number"
    );
}

#[tokio::test]
async fn worktree_list_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "--no-pager",
        "--no-optional-locks",
        "worktree",
        "list",
        "--porcelain",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "worktree list start event should not be enqueued (readonly invocation)"
    );
    assert!(
        payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
        "worktree list start event must not receive an ingest sequence number"
    );
}

#[tokio::test]
async fn branch_show_current_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&["git", "branch", "--show-current"]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "branch --show-current start event should not be enqueued"
    );
    assert!(
        payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
        "branch --show-current must not receive an ingest sequence number"
    );
}

#[tokio::test]
async fn diff_numstat_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "-c",
        "core.fsmonitor=false",
        "--no-pager",
        "diff",
        "--numstat",
        "--no-renames",
        "HEAD",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "diff --numstat start event should not be enqueued"
    );
}

#[tokio::test]
async fn for_each_ref_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "--no-pager",
        "for-each-ref",
        "refs/heads/**/*",
        "refs/remotes/**/*",
        "--format",
        "%(HEAD)%00%(objectname)",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "for-each-ref start event should not be enqueued"
    );
}

#[tokio::test]
async fn cat_file_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "--no-optional-locks",
        "cat-file",
        "--batch-check=%(objectname)",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        !should_enqueue,
        "cat-file start event should not be enqueued"
    );
}

#[tokio::test]
async fn show_commit_start_event_is_not_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&[
        "git",
        "--no-optional-locks",
        "show",
        "--no-patch",
        "--format=%H%x00%B%x00%at",
        "07270e1489439d6b36fcb2a4198d2fb68e37727c",
    ]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(!should_enqueue, "show start event should not be enqueued");
}

#[tokio::test]
async fn mutating_commit_start_event_is_enqueued() {
    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.start_trace_ingest_worker().unwrap();
    let mut payload = make_start_payload(&["git", "commit", "-m", "test commit"]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        should_enqueue,
        "commit start event should be enqueued (mutating)"
    );
    assert!(
        payload.get(TRACE_INGEST_SEQ_FIELD).is_none(),
        "mutating event must not receive an ingest sequence number before enqueue capacity is reserved"
    );
    assert_eq!(
        coord.next_trace_ingest_seq.load(Ordering::Acquire),
        0,
        "prepare must not allocate an ingest sequence"
    );
    coord
        .enqueue_trace_payload(payload)
        .expect("mutating event should enqueue");
    assert!(
        coord.next_trace_ingest_seq.load(Ordering::Acquire) > 0,
        "enqueue must allocate an ingest sequence number"
    );
    coord.request_shutdown();
}

#[tokio::test]
async fn mutating_pending_root_is_created_when_repo_and_argv_arrive_on_different_events() {
    let coord = ActorDaemonCoordinator::new();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(temp.path())
        .arg("init")
        .arg("repo")
        .output()
        .expect("git init should run");
    assert!(
        init.status.success(),
        "git init failed: {}",
        String::from_utf8_lossy(&init.stderr)
    );

    let sid = "20260411T120000.000000-Psid-split-metadata";
    let mut def_repo = serde_json::json!({
        "event": "def_repo",
        "sid": sid,
        "worktree": repo,
        "time_ns": 1u64,
    });
    assert!(coord.prepare_trace_payload_for_ingest(&mut def_repo));
    coord
        .apply_trace_payload_to_state(def_repo)
        .await
        .expect("def_repo should ingest");
    assert!(
        !coord
            .pending_root_slots_by_root
            .lock()
            .unwrap()
            .contains_key(sid),
        "repo-only metadata is not enough to sequence a command"
    );

    let mut start = serde_json::json!({
        "event": "start",
        "sid": sid,
        "argv": ["git", "reset", "--soft", "HEAD~1"],
        "time_ns": 2u64,
    });
    assert!(coord.prepare_trace_payload_for_ingest(&mut start));
    coord
        .apply_trace_payload_to_state(start)
        .await
        .expect("start should ingest");

    assert!(
        coord
            .pending_root_slots_by_root
            .lock()
            .unwrap()
            .contains_key(sid),
        "mutating roots must be sequenced once argv and repo metadata are both known, even when they arrive on different events"
    );
}

#[tokio::test]
async fn mutating_trace_payload_captures_repo_reflog_start_offsets() {
    let coord = ActorDaemonCoordinator::new();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    let git_dir = repo.join(".git");
    let head_log = git_dir.join("logs/HEAD");
    let stash_log = repo.join(".git/logs/refs/stash");
    let branch_log = repo.join(".git/logs/refs/heads/main");
    std::fs::create_dir_all(head_log.parent().unwrap()).unwrap();
    crate::operations::git::test_utils::seed_valid_git_dir(&git_dir);
    std::fs::create_dir_all(stash_log.parent().unwrap()).unwrap();
    std::fs::create_dir_all(branch_log.parent().unwrap()).unwrap();
    let old_head_reflog = b"old HEAD reflog entry\n";
    let old_reflog = b"old stash reflog entry\n";
    let old_branch_reflog = b"old branch reflog entry\n";
    std::fs::write(&head_log, old_head_reflog).unwrap();
    std::fs::write(&stash_log, old_reflog).unwrap();
    std::fs::write(&branch_log, old_branch_reflog).unwrap();
    let mut payload = serde_json::json!({
        "event": "start",
        "sid": "20260411T120000.000000-Psid-reflog",
        "argv": ["git", "reset", "--hard", "HEAD~1"],
        "worktree": repo,
    });

    assert!(coord.prepare_trace_payload_for_ingest(&mut payload));

    let offsets = payload
        .get(TRACE_ROOT_REFLOG_START_OFFSETS_FIELD)
        .and_then(Value::as_object)
        .expect("mutating trace payload should include reflog start offsets");
    let head_key = format!(
        "worktree:{}:HEAD",
        git_dir.canonicalize().unwrap().to_string_lossy()
    );
    assert_eq!(
        offsets.get(&head_key).and_then(Value::as_u64),
        Some(old_head_reflog.len() as u64)
    );
    assert_eq!(
        offsets.get("common:refs/stash").and_then(Value::as_u64),
        Some(old_reflog.len() as u64)
    );
    assert_eq!(
        offsets
            .get("common:refs/heads/main")
            .and_then(Value::as_u64),
        Some(old_branch_reflog.len() as u64)
    );
}

#[tokio::test]
async fn mutating_stash_pop_start_event_is_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&["git", "stash", "pop"]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        should_enqueue,
        "stash pop start event should be enqueued (mutating)"
    );
}

#[tokio::test]
async fn mutating_worktree_add_start_event_is_enqueued() {
    let coord = ActorDaemonCoordinator::new();
    let mut payload = make_start_payload(&["git", "worktree", "add", "/tmp/branch", "branch"]);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
    assert!(
        should_enqueue,
        "worktree add start event should be enqueued (mutating)"
    );
}

#[tokio::test]
async fn readonly_atexit_event_is_not_enqueued_after_readonly_start() {
    let coord = ActorDaemonCoordinator::new();
    let sid = "20260411T120000.000000-Psid1";

    // Process start event first — marks root as read-only
    let mut start = make_start_payload(&["git", "status"]);
    // Override sid to match
    start["sid"] = serde_json::json!(sid);
    coord.prepare_trace_payload_for_ingest(&mut start);

    // atexit for same root should also be skipped
    let mut atexit = make_atexit_payload(sid);
    let should_enqueue = coord.prepare_trace_payload_for_ingest(&mut atexit);
    assert!(
        !should_enqueue,
        "atexit for readonly root should not be enqueued"
    );
}

/// Performance invariant: 10,000 readonly start events must be processed
/// (and discarded) in under 200ms.  This guards against regressions that
/// re-introduce the >1-minute backlog seen with Zed's ~40 invocations/sec.
#[tokio::test]
async fn readonly_flood_1000_events_processed_in_under_200ms() {
    let coord = ActorDaemonCoordinator::new();
    let start = std::time::Instant::now();
    for i in 0..1000u64 {
        let sid = format!("20260411T120000.000000-P{:016x}", i);
        let mut payload = serde_json::json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "-c", "core.fsmonitor=false", "--no-pager",
                     "--no-optional-locks", "status", "--porcelain=v1",
                     "--untracked-files=all", "--no-renames", "-z", "."],
        });
        let enqueue = coord.prepare_trace_payload_for_ingest(&mut payload);
        assert!(!enqueue, "status must never be enqueued");
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed.as_millis() < 200,
        "processing 1000 readonly events took {}ms (> 200ms budget)",
        elapsed.as_millis()
    );
    assert_eq!(
        coord.queued_trace_payloads.load(Ordering::Relaxed),
        0,
        "no readonly events should reach the ingest queue"
    );
}

/// Ensure a stash-list flood (3208 real-world invocations from Zed)
/// leaves the ingest queue empty.
#[tokio::test]
async fn stash_list_flood_leaves_queue_empty() {
    let coord = ActorDaemonCoordinator::new();
    for i in 0..1000u64 {
        let sid = format!("20260411T120000.000000-P{:016x}", i);
        let mut payload = serde_json::json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "-c", "core.fsmonitor=false", "--no-pager",
                     "stash", "list", "--pretty=format:%gd%x00%H%x00%ct%x00%s"],
        });
        let _ = coord.prepare_trace_payload_for_ingest(&mut payload);
    }
    assert_eq!(
        coord.queued_trace_payloads.load(Ordering::Relaxed),
        0,
        "stash list flood must not fill the ingest queue"
    );
}

/// Ensure a worktree-list flood leaves the ingest queue empty.
#[tokio::test]
async fn worktree_list_flood_leaves_queue_empty() {
    let coord = ActorDaemonCoordinator::new();
    for i in 0..1000u64 {
        let sid = format!("20260411T120000.000000-P{:016x}", i);
        let mut payload = serde_json::json!({
            "event": "start",
            "sid": sid,
            "argv": ["git", "--no-pager", "--no-optional-locks",
                     "worktree", "list", "--porcelain"],
        });
        let _ = coord.prepare_trace_payload_for_ingest(&mut payload);
    }
    assert_eq!(
        coord.queued_trace_payloads.load(Ordering::Relaxed),
        0,
        "worktree list flood must not fill the ingest queue"
    );
}
