//! Scheduled family-drain tests for `ActorDaemonCoordinator` (split from
//! `tests_coordinator.rs` to respect the file length cap).

use super::*;
use crate::model::checkpoint_request::CheckpointRequest;
use crate::model::working_log::CheckpointKind;
use crate::operations::daemon::git_backend::GitBackend;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
async fn registered_coordinator_schedules_drains_and_completes_blocked_checkpoints() {
    use crate::model::checkpoint_request::{BaseCommit, CheckpointFile, PreparedPathRole};

    let coord = Arc::new(ActorDaemonCoordinator::new());
    coord.register_self();
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let init = std::process::Command::new("git")
        .arg("-C")
        .arg(&repo)
        .arg("init")
        .output()
        .expect("git init should run");
    assert!(init.status.success());
    std::fs::write(repo.join("test.txt"), "scheduled drain content\n").unwrap();

    let family = coord.backend.resolve_family(&repo).unwrap().0;
    let root_sid = "20260411T120000.000000-Psid-scheduled-root";
    coord
        .append_pending_root_entry(&family, root_sid, 1)
        .unwrap();

    let request = CheckpointRequest {
        trace_id: "scheduled-checkpoint".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![CheckpointFile {
            path: PathBuf::from("test.txt"),
            content: Some("scheduled drain content\n".to_string()),
            repo_work_dir: repo.clone(),
            base_commit: BaseCommit::Initial,
        }],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };
    let mut checkpoint = {
        let coord = coord.clone();
        tokio::spawn(async move { coord.ingest_checkpoint_payload(request).await })
    };
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut checkpoint)
            .await
            .is_err(),
        "checkpoint must stay blocked behind the pending root"
    );

    // Releasing the root schedules the family drain on its own task; the
    // blocked checkpoint must complete through that scheduled drain.
    coord
        .replace_pending_root_entry(root_sid, FamilySequencerEntry::Canceled)
        .await
        .unwrap();
    let response = tokio::time::timeout(Duration::from_secs(2), checkpoint)
        .await
        .expect("scheduled drain should complete the blocked checkpoint")
        .expect("checkpoint task should not panic")
        .expect("checkpoint request should succeed");
    assert!(
        response.ok,
        "checkpoint response should be ok: {response:?}"
    );

    // The coalescing map must drain back to empty once the task finishes.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        let empty = coord.scheduled_family_drains.lock().unwrap().is_empty();
        if empty {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "scheduled family drain task did not unregister itself"
        );
        tokio::task::yield_now().await;
    }
}

/// The transient-state GC must never remove a family's exec lock while a
/// drain holds it: handing the next acquirer a fresh mutex would let two
/// drains of the same family run concurrently, breaking the per-family
/// serialization every side effect relies on. Idle locks (held by nobody)
/// are the ones the GC exists to prune.
#[tokio::test]
async fn gc_keeps_held_exec_locks_and_prunes_idle_ones() {
    let coord = ActorDaemonCoordinator::new();

    let held = coord.side_effect_exec_lock("family-held").unwrap();
    let _guard = held.lock().await;
    let idle = coord.side_effect_exec_lock("family-idle").unwrap();
    drop(idle);

    coord.gc_stale_family_state();

    let held_after = coord.side_effect_exec_lock("family-held").unwrap();
    assert!(
        Arc::ptr_eq(&held, &held_after),
        "GC replaced a held exec lock; a concurrent acquirer would no longer \
         be excluded by the drain still holding the old lock"
    );
    let idle_after = coord.side_effect_exec_lock("family-idle").unwrap();
    assert_eq!(
        Arc::strong_count(&idle_after),
        2,
        "idle exec locks should have been pruned and recreated fresh"
    );
}
