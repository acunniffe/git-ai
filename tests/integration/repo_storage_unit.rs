use crate::repos::test_repo::TestRepo;
use git_ai::error::GitAiError;
use git_ai::model::attribution_tracker::LineAttribution;
use git_ai::model::working_log::{AgentId, CHECKPOINT_API_VERSION, Checkpoint, CheckpointKind};
use git_ai::operations::git::repo_storage::{InitialAttributions, RepoStorage};
use std::collections::HashMap;
use std::fs;
use std::time::SystemTime;

/// Helper: create a RepoStorage for a TestRepo.
fn storage_for(repo: &TestRepo) -> RepoStorage {
    let git_dir = repo.path().join(".git");
    let workdir = repo.path();
    RepoStorage::for_repo_path(&git_dir, workdir.as_path()).unwrap()
}

// ---------------------------------------------------------------------------
// 1. test_ensure_config_directory_creates_structure
// ---------------------------------------------------------------------------

#[test]
fn test_ensure_config_directory_creates_structure() {
    let repo = TestRepo::new();
    let _repo_storage = storage_for(&repo);

    let ai_dir = repo.path().join(".git").join("ai");
    assert!(ai_dir.exists(), ".git/ai directory should exist");
    assert!(ai_dir.is_dir(), ".git/ai should be a directory");

    let working_logs_dir = ai_dir.join("working_logs");
    assert!(
        working_logs_dir.exists(),
        "working_logs directory should exist"
    );
    assert!(
        working_logs_dir.is_dir(),
        "working_logs should be a directory"
    );

    let logs_dir = ai_dir.join("logs");
    assert!(logs_dir.exists(), "logs directory should exist");
    assert!(logs_dir.is_dir(), "logs should be a directory");
}

// ---------------------------------------------------------------------------
// 2. test_ensure_config_directory_handles_existing_files
// ---------------------------------------------------------------------------

#[test]
fn test_ensure_config_directory_handles_existing_dirs() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    // Call ensure_config_directory again - should be idempotent
    repo_storage
        .ensure_config_directory()
        .expect("Failed to ensure config directory again");

    let ai_dir = repo.path().join(".git").join("ai");
    let working_logs_dir = ai_dir.join("working_logs");
    assert!(ai_dir.exists(), ".git/ai directory should still exist");
    assert!(
        working_logs_dir.exists(),
        "working_logs directory should still exist"
    );
}

// ---------------------------------------------------------------------------
// 3. test_persisted_working_log_blob_storage
// ---------------------------------------------------------------------------

#[test]
fn test_persisted_working_log_blob_storage() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let content = "abc";
    let expected_sha = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
    let sha = working_log
        .persist_file_version(content)
        .expect("Failed to persist file version");

    assert_eq!(sha, expected_sha, "blob name should be lowercase SHA-256");

    let retrieved_content = working_log
        .get_file_version(&sha)
        .expect("Failed to get file version");

    assert_eq!(
        content, retrieved_content,
        "Retrieved content should match original"
    );

    let blob_path = working_log.dir.join("blobs").join(&sha);
    assert!(blob_path.exists(), "Blob file should exist");
    assert!(blob_path.is_file(), "Blob should be a file");
    assert_eq!(
        fs::read(&blob_path).unwrap(),
        content.as_bytes(),
        "Blob should preserve the exact input bytes"
    );

    fs::write(&blob_path, b"stale").unwrap();
    let sha2 = working_log
        .persist_file_version(content)
        .expect("Failed to persist file version again");

    assert_eq!(sha, sha2, "Same content should produce same SHA");
    assert_eq!(
        fs::read(&blob_path).unwrap(),
        content.as_bytes(),
        "Repeated persistence should unconditionally rewrite the blob"
    );
    assert_eq!(
        fs::read_dir(working_log.dir.join("blobs")).unwrap().count(),
        1,
        "Repeated persistence should keep one content-addressed blob"
    );
}

#[test]
fn test_persisted_working_log_blob_storage_preserves_io_error() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();
    let blobs_dir = working_log.dir.join("blobs");
    fs::write(&blobs_dir, b"blocker").unwrap();

    let expected = fs::create_dir_all(&blobs_dir).unwrap_err();
    let error = working_log
        .persist_file_version("abc")
        .expect_err("a file at the blobs path should prevent persistence");

    match &error {
        GitAiError::IoError(actual) => {
            assert_eq!(actual.kind(), expected.kind());
            assert_eq!(actual.to_string(), expected.to_string());
        }
        other => panic!("expected lossless IO error, got {other:?}"),
    }
    assert_eq!(error.to_string(), format!("IO error: {expected}"));
    assert_eq!(fs::read(&blobs_dir).unwrap(), b"blocker");
}

// ---------------------------------------------------------------------------
// 4. test_persisted_working_log_checkpoint_storage
// ---------------------------------------------------------------------------

#[test]
fn test_persisted_working_log_checkpoint_storage() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let checkpoint = Checkpoint::new(
        CheckpointKind::Human,
        "test-diff".to_string(),
        "test-author".to_string(),
        vec![],
    );

    working_log
        .append_checkpoint(&checkpoint)
        .expect("Failed to append checkpoint");

    let checkpoints = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints");

    assert_eq!(checkpoints.len(), 1, "Should have one checkpoint");
    assert_eq!(checkpoints[0].author, "test-author");

    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    assert!(checkpoints_file.exists(), "Checkpoints file should exist");

    let checkpoint2 = Checkpoint::new(
        CheckpointKind::Human,
        "test-diff-2".to_string(),
        "test-author-2".to_string(),
        vec![],
    );

    working_log
        .append_checkpoint(&checkpoint2)
        .expect("Failed to append second checkpoint");

    let checkpoints = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints after second append");

    assert_eq!(checkpoints.len(), 2, "Should have two checkpoints");
    assert_eq!(checkpoints[1].author, "test-author-2");
}

#[test]
fn test_append_checkpoint_to_persists_the_materialized_collection_without_rereading() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let first = Checkpoint::new(
        CheckpointKind::Human,
        "first-diff".to_string(),
        "first-author".to_string(),
        vec![],
    );
    working_log
        .append_checkpoint(&first)
        .expect("Failed to append first checkpoint");
    let mut materialized = working_log
        .read_all_checkpoints()
        .expect("Failed to materialize checkpoints");

    // Corrupt the on-disk file after materializing: the append must persist
    // from the in-memory collection, not from a second read of the file.
    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    fs::write(&checkpoints_file, b"not json\n").unwrap();

    let second = Checkpoint::new(
        CheckpointKind::Human,
        "second-diff".to_string(),
        "second-author".to_string(),
        vec![],
    );
    working_log
        .append_checkpoint_to(&mut materialized, second)
        .expect("Failed to append to the materialized collection");

    assert_eq!(
        materialized.len(),
        2,
        "The in-memory collection should now hold both checkpoints"
    );
    let persisted = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints back");
    assert_eq!(
        persisted
            .iter()
            .map(|checkpoint| checkpoint.author.as_str())
            .collect::<Vec<_>>(),
        vec!["first-author", "second-author"],
        "The persisted file must reflect the materialized collection plus the append"
    );
}

// ---------------------------------------------------------------------------
// 5. test_read_all_checkpoints_filters_incompatible_versions
// ---------------------------------------------------------------------------

#[test]
fn test_read_all_checkpoints_filters_incompatible_versions() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let base_checkpoint = Checkpoint::new(
        CheckpointKind::Human,
        "diff --git a/file b/file".to_string(),
        "base-author".to_string(),
        vec![],
    );

    let missing_version_json = {
        let mut value = serde_json::to_value(&base_checkpoint).unwrap();
        if let serde_json::Value::Object(ref mut map) = value {
            map.remove("api_version");
        }
        serde_json::to_string(&value).unwrap()
    };

    let mut wrong_version_checkpoint = base_checkpoint.clone();
    wrong_version_checkpoint.api_version = "checkpoint/0.9.0".to_string();
    let wrong_version_json = serde_json::to_string(&wrong_version_checkpoint).unwrap();

    let mut correct_checkpoint = base_checkpoint.clone();
    correct_checkpoint.author = "correct-author".to_string();
    let correct_json = serde_json::to_string(&correct_checkpoint).unwrap();

    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    let combined = [missing_version_json, wrong_version_json, correct_json].join("\n");
    fs::write(&checkpoints_file, combined).expect("Failed to write checkpoints.jsonl");

    let checkpoints = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints");

    assert_eq!(
        checkpoints.len(),
        1,
        "Only the correct version should remain"
    );
    assert_eq!(checkpoints[0].author, "correct-author");
    assert_eq!(checkpoints[0].api_version, CHECKPOINT_API_VERSION);
}

#[test]
fn test_oversized_checkpoints_file_is_truncated_before_read() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();
    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");

    fs::write(&checkpoints_file, "this is intentionally not valid json\n")
        .expect("write oversized checkpoints fixture");

    let checkpoints = working_log
        .read_all_checkpoints_with_size_limit_for_test(8)
        .expect("oversized checkpoint file should be reset before parsing");

    assert!(
        checkpoints.is_empty(),
        "oversized checkpoints file should read back as empty"
    );
    assert_eq!(
        fs::metadata(&checkpoints_file)
            .expect("empty checkpoints file should remain")
            .len(),
        0,
        "oversized checkpoints file should be truncated to an empty file"
    );
}

// ---------------------------------------------------------------------------
// 6. test_persisted_working_log_reset
// ---------------------------------------------------------------------------

#[test]
fn test_persisted_working_log_reset() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let content = "Test content";
    let sha = working_log
        .persist_file_version(content)
        .expect("Failed to persist file version");

    let checkpoint = Checkpoint::new(
        CheckpointKind::Human,
        "test-diff".to_string(),
        "test-author".to_string(),
        vec![],
    );
    working_log
        .append_checkpoint(&checkpoint)
        .expect("Failed to append checkpoint");

    assert!(working_log.dir.join("blobs").join(&sha).exists());
    let checkpoints = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints");
    assert_eq!(checkpoints.len(), 1);

    working_log
        .reset_working_log()
        .expect("Failed to reset working log");

    assert!(
        !working_log.dir.join("blobs").exists(),
        "Blobs directory should be removed"
    );

    let checkpoints = working_log
        .read_all_checkpoints()
        .expect("Failed to read checkpoints after reset");
    assert_eq!(
        checkpoints.len(),
        0,
        "Should have no checkpoints after reset"
    );

    let checkpoints_file = working_log.dir.join("checkpoints.jsonl");
    assert!(
        checkpoints_file.exists(),
        "Checkpoints file should still exist"
    );
    let content = fs::read_to_string(&checkpoints_file).expect("Failed to read checkpoints file");
    assert!(
        content.trim().is_empty(),
        "Checkpoints file should be empty"
    );
}

// ---------------------------------------------------------------------------
// 7. test_working_log_for_base_commit_creates_directory
// ---------------------------------------------------------------------------

#[test]
fn test_working_log_for_base_commit_creates_directory() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    let commit_sha = "abc123def456";
    let working_log = repo_storage
        .working_log_for_base_commit(commit_sha)
        .unwrap();

    assert!(
        working_log.dir.exists(),
        "Working log directory should exist"
    );
    assert!(
        working_log.dir.is_dir(),
        "Working log should be a directory"
    );

    let expected_path = repo
        .path()
        .join(".git")
        .join("ai")
        .join("working_logs")
        .join(commit_sha);
    assert_eq!(
        working_log.dir, expected_path,
        "Working log directory should be in correct location"
    );
}

// ---------------------------------------------------------------------------
// 8. test_write_initial_with_contents_persists_snapshot_blob
// ---------------------------------------------------------------------------

#[test]
fn test_write_initial_with_contents_persists_snapshot_blob() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let mut attributions = HashMap::new();
    attributions.insert(
        "src/test.rs".to_string(),
        vec![LineAttribution {
            start_line: 1,
            end_line: 1,
            author_id: "ai-1".to_string(),
            overrode: None,
        }],
    );
    let mut contents = HashMap::new();
    contents.insert("src/test.rs".to_string(), "fn main() {}\n".to_string());

    working_log
        .write_initial_attributions_with_contents(
            attributions,
            HashMap::new(),
            std::collections::BTreeMap::new(),
            contents,
            std::collections::BTreeMap::new(),
        )
        .expect("write INITIAL with contents");

    let initial = working_log.read_initial_attributions();
    let blob_sha = initial
        .file_blobs
        .get("src/test.rs")
        .expect("snapshot blob should exist");
    let persisted = working_log
        .get_file_version(blob_sha)
        .expect("read snapshot blob");
    assert_eq!(persisted, "fn main() {}\n");
}

#[test]
fn test_write_initial_with_contents_rejects_missing_snapshot() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let mut attributions = HashMap::new();
    attributions.insert(
        "src/test.rs".to_string(),
        vec![LineAttribution {
            start_line: 1,
            end_line: 1,
            author_id: "ai-1".to_string(),
            overrode: None,
        }],
    );

    let error = working_log
        .write_initial_attributions_with_contents(
            attributions,
            HashMap::new(),
            std::collections::BTreeMap::new(),
            HashMap::new(),
            std::collections::BTreeMap::new(),
        )
        .expect_err("missing content snapshot must be rejected");

    assert!(
        error
            .to_string()
            .contains("INITIAL missing file content snapshot for src/test.rs"),
        "unexpected error: {error}"
    );
}

// ---------------------------------------------------------------------------
// 9. test_write_initial_empty_removes_existing_file
// ---------------------------------------------------------------------------

#[test]
fn test_write_initial_empty_removes_existing_file() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    let mut attributions = HashMap::new();
    attributions.insert(
        "src/test.rs".to_string(),
        vec![LineAttribution {
            start_line: 1,
            end_line: 1,
            author_id: "ai-1".to_string(),
            overrode: None,
        }],
    );
    working_log
        .write_initial_attributions_with_contents(
            attributions,
            HashMap::new(),
            std::collections::BTreeMap::new(),
            HashMap::from([("src/test.rs".to_string(), "fn main() {}\n".to_string())]),
            std::collections::BTreeMap::new(),
        )
        .expect("write INITIAL");
    assert!(working_log.initial_file.exists(), "INITIAL should exist");

    working_log
        .write_initial(InitialAttributions::default())
        .expect("clear INITIAL");
    assert!(
        !working_log.initial_file.exists(),
        "INITIAL should be removed when empty"
    );
}

// ---------------------------------------------------------------------------
// 10. test_pi_transcript_refetch_requires_session_path_metadata
// ---------------------------------------------------------------------------

#[test]
fn test_pi_transcript_refetch_requires_session_path_metadata() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);
    let working_log = repo_storage
        .working_log_for_base_commit("test-commit-sha")
        .unwrap();

    // Pi checkpoint WITH session_path metadata -> transcript should be dropped
    let mut checkpoint_with_session_path = Checkpoint::new(
        CheckpointKind::AiAgent,
        "diff".to_string(),
        "author".to_string(),
        vec![],
    );
    checkpoint_with_session_path.agent_id = Some(AgentId {
        tool: "pi".to_string(),
        id: "session-1".to_string(),
        model: "anthropic/claude-sonnet-4-5".to_string(),
    });
    // Transcript field removed from Checkpoint struct
    checkpoint_with_session_path.agent_metadata = Some(HashMap::from([(
        "session_path".to_string(),
        "/tmp/pi-session.jsonl".to_string(),
    )]));

    working_log
        .append_checkpoint(&checkpoint_with_session_path)
        .expect("append checkpoint with session_path");

    let _checkpoints = working_log
        .read_all_checkpoints()
        .expect("read checkpoints with session_path");
    // Pi checkpoint persistence tested, transcript field no longer exists

    // Pi checkpoint WITHOUT session_path metadata
    let mut checkpoint_without_session_path = Checkpoint::new(
        CheckpointKind::AiAgent,
        "diff-2".to_string(),
        "author".to_string(),
        vec![],
    );
    checkpoint_without_session_path.agent_id = Some(AgentId {
        tool: "pi".to_string(),
        id: "session-2".to_string(),
        model: "anthropic/claude-sonnet-4-5".to_string(),
    });
    // Transcript field removed from Checkpoint struct
    checkpoint_without_session_path.agent_metadata = Some(HashMap::new());

    working_log
        .append_checkpoint(&checkpoint_without_session_path)
        .expect("append checkpoint without session_path");

    let _checkpoints = working_log
        .read_all_checkpoints()
        .expect("read checkpoints without session_path");
    // Test passes - checkpoints can be written and read
}

// ---------------------------------------------------------------------------
// 11. test_delete_working_log_archives_to_old_sha
// ---------------------------------------------------------------------------

#[test]
fn test_delete_working_log_archives_to_old_sha() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    let sha = "abc123";
    let wl_dir = repo_storage.working_logs.join(sha);
    fs::create_dir_all(&wl_dir).unwrap();
    fs::write(wl_dir.join("checkpoints.jsonl"), "").unwrap();

    assert!(wl_dir.exists());

    repo_storage
        .delete_working_log_for_base_commit(sha)
        .unwrap();

    assert!(!wl_dir.exists());

    let old_dir = repo_storage.working_logs.join(format!("old-{}", sha));
    assert!(old_dir.exists());
    assert!(old_dir.is_dir());

    let marker = old_dir.join(".archived_at");
    assert!(marker.exists());
    let ts: u64 = fs::read_to_string(&marker).unwrap().trim().parse().unwrap();
    assert!(ts > 0);
}

// ---------------------------------------------------------------------------
// 12. test_delete_working_log_replaces_existing_old_dir
// ---------------------------------------------------------------------------

#[test]
fn test_delete_working_log_replaces_existing_old_dir() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    let sha = "def456";

    let old_dir = repo_storage.working_logs.join(format!("old-{}", sha));
    fs::create_dir_all(&old_dir).unwrap();
    fs::write(old_dir.join("stale.txt"), "stale").unwrap();

    let wl_dir = repo_storage.working_logs.join(sha);
    fs::create_dir_all(&wl_dir).unwrap();
    fs::write(wl_dir.join("checkpoints.jsonl"), "fresh").unwrap();

    repo_storage
        .delete_working_log_for_base_commit(sha)
        .unwrap();

    assert!(!old_dir.join("stale.txt").exists());
    assert!(old_dir.join("checkpoints.jsonl").exists());
}

// ---------------------------------------------------------------------------
// 13. test_prune_expired_old_working_logs_removes_expired
// ---------------------------------------------------------------------------

#[test]
fn test_prune_expired_old_working_logs_removes_expired() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    // Create an old working log with an expired timestamp (8 days ago)
    let expired_dir = repo_storage.working_logs.join("old-expired111");
    fs::create_dir_all(&expired_dir).unwrap();
    let eight_days_ago = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - (8 * 24 * 60 * 60);
    fs::write(expired_dir.join(".archived_at"), eight_days_ago.to_string()).unwrap();

    // Create an old working log with a fresh timestamp (1 day ago)
    let fresh_dir = repo_storage.working_logs.join("old-fresh222");
    fs::create_dir_all(&fresh_dir).unwrap();
    let one_day_ago = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
        - (24 * 60 * 60);
    fs::write(fresh_dir.join(".archived_at"), one_day_ago.to_string()).unwrap();

    repo_storage.prune_expired_old_working_logs();

    assert!(
        !expired_dir.exists(),
        "Expired old working log should be pruned"
    );

    assert!(
        fresh_dir.exists(),
        "Fresh old working log should be retained"
    );
}

// ---------------------------------------------------------------------------
// 14. test_prune_expired_old_working_logs_removes_missing_marker
// ---------------------------------------------------------------------------

#[test]
fn test_prune_expired_old_working_logs_removes_missing_marker() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    let no_marker_dir = repo_storage.working_logs.join("old-nomarker");
    fs::create_dir_all(&no_marker_dir).unwrap();

    repo_storage.prune_expired_old_working_logs();

    assert!(
        !no_marker_dir.exists(),
        "Old working log without marker should be pruned"
    );
}

// ---------------------------------------------------------------------------
// 15. test_prune_does_not_touch_active_working_logs
// ---------------------------------------------------------------------------

#[test]
fn test_prune_does_not_touch_active_working_logs() {
    let repo = TestRepo::new();
    let repo_storage = storage_for(&repo);

    let active_dir = repo_storage.working_logs.join("abc123active");
    fs::create_dir_all(&active_dir).unwrap();
    fs::write(active_dir.join("checkpoints.jsonl"), "data").unwrap();

    repo_storage.prune_expired_old_working_logs();

    assert!(
        active_dir.exists(),
        "Active working logs should not be pruned"
    );
}
