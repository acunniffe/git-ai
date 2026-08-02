use std::sync::Arc;

use super::TraceNormalizer;
use super::tests_lifecycle::{MockBackend, atexit_payload};

#[test]
fn normalizer_keeps_the_primary_worktree_after_a_secondary_def_repo() {
    let temp = tempfile::tempdir().expect("create tempdir");
    let parent = temp.path().join("parent");
    let nested = parent.join("nested");
    crate::operations::git::test_utils::seed_valid_git_dir(&parent.join(".git"));
    crate::operations::git::test_utils::seed_valid_git_dir(&nested.join(".git"));

    let mut normalizer = TraceNormalizer::new(Arc::new(MockBackend::default()));
    let start = serde_json::json!({
        "event": "start",
        "sid": "nested-def-repo",
        "ts": 1,
        "argv": ["git", "commit", "-m", "message"],
        "worktree": parent,
    });
    let primary = serde_json::json!({
        "event": "def_repo",
        "sid": "nested-def-repo",
        "ts": 2,
        "repo": 1,
        "worktree": parent,
    });
    let mut secondary = serde_json::json!({
        "event": "def_repo",
        "sid": "nested-def-repo",
        "ts": 3,
        "repo": 2,
        "worktree": nested,
    });
    secondary.as_object_mut().unwrap().insert(
        crate::operations::daemon::TRACE_ROOT_REFLOG_START_OFFSETS_FIELD.to_string(),
        serde_json::json!({"HEAD": 42_u64}),
    );

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&primary).unwrap().is_none());
    assert!(normalizer.ingest_payload(&secondary).unwrap().is_none());
    let command = normalizer
        .ingest_payload(&atexit_payload("nested-def-repo", 4))
        .unwrap()
        .expect("command should normalize");

    assert_eq!(command.worktree.as_deref(), Some(parent.as_path()));
    assert_eq!(command.reflog_start_offsets.get("HEAD"), Some(&42));
}
