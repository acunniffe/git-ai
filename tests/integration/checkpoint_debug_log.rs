use crate::repos::test_repo::TestRepo;

fn checkpoint_debug_log_dir(repo: &TestRepo) -> std::path::PathBuf {
    repo.test_home_path()
        .join(".git-ai")
        .join("internal")
        .join("checkpoint-debug-logs")
}

#[test]
fn test_checkpoint_debug_log_writes_when_enabled() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });

    let file_path = repo.path().join("test.txt");
    std::fs::write(&file_path, "hello\n").unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    let log_dir = checkpoint_debug_log_dir(&repo);
    assert!(log_dir.exists(), "checkpoint-debug-logs dir should exist");

    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1, "should have exactly one daily log file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777,
            0o700,
            "the raw-input debug log directory must be private"
        );
        assert_eq!(
            entries[0].metadata().unwrap().permissions().mode() & 0o777,
            0o600,
            "the raw-input debug log file must be private"
        );
    }

    let content = std::fs::read_to_string(entries[0].path()).unwrap();
    let line: serde_json::Value = serde_json::from_str(content.trim()).unwrap();
    assert_eq!(line["preset_name"], "mock_known_human");
    assert!(line["trace_id"].is_string());
    assert!(line["timestamp"].is_string());
    assert!(line["event_count"].is_number());
    assert!(line["requests"].is_array());
}

#[cfg(unix)]
#[test]
fn test_checkpoint_debug_log_tightens_existing_permissions_before_append() {
    use std::os::unix::fs::PermissionsExt;

    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let log_dir = checkpoint_debug_log_dir(&repo);
    std::fs::create_dir_all(&log_dir).unwrap();
    std::fs::set_permissions(&log_dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let log_path = log_dir.join(format!("{}.log", chrono::Utc::now().format("%Y-%m-%d")));
    std::fs::write(&log_path, "existing entry\n").unwrap();
    std::fs::set_permissions(&log_path, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::write(repo.path().join("test.txt"), "hello\n").unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    assert_eq!(
        std::fs::metadata(&log_dir).unwrap().permissions().mode() & 0o777,
        0o700,
        "an existing raw-input debug log directory must be tightened"
    );
    assert_eq!(
        std::fs::metadata(&log_path).unwrap().permissions().mode() & 0o777,
        0o600,
        "an existing raw-input debug log file must be tightened"
    );
    assert!(
        std::fs::read_to_string(&log_path).unwrap().lines().count() >= 2,
        "the checkpoint entry should be appended after permissions are tightened"
    );
}

#[test]
fn test_checkpoint_debug_log_does_not_write_when_disabled() {
    let repo = TestRepo::new();

    let file_path = repo.path().join("test.txt");
    std::fs::write(&file_path, "hello\n").unwrap();

    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    let log_dir = checkpoint_debug_log_dir(&repo);
    assert!(
        !log_dir.exists(),
        "checkpoint-debug-logs dir should NOT exist when flag is off"
    );
}

#[test]
fn test_checkpoint_debug_log_appends_multiple_entries() {
    let mut repo = TestRepo::new();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });

    let file_path = repo.path().join("test.txt");
    std::fs::write(&file_path, "hello\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "test.txt"])
        .unwrap();

    std::fs::write(&file_path, "hello\nworld\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "test.txt"]).unwrap();

    let log_dir = checkpoint_debug_log_dir(&repo);

    let entries: Vec<_> = std::fs::read_dir(&log_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    assert_eq!(entries.len(), 1);

    let content = std::fs::read_to_string(entries[0].path()).unwrap();
    let lines: Vec<&str> = content.trim().lines().collect();
    assert_eq!(lines.len(), 2, "should have two JSONL entries");

    let first: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
    let second: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(first["preset_name"], "mock_known_human");
    assert_eq!(second["preset_name"], "mock_ai");
}

#[test]
fn test_checkpoint_debug_log_does_not_persist_denied_input() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(Vec::new());
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    std::fs::write(repo.path().join("private.txt"), "sensitive input\n").unwrap();

    let output = repo
        .git_ai(&["checkpoint", "mock_ai", "private.txt"])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("no repositories are allowed"));
    assert!(
        !checkpoint_debug_log_dir(&repo).exists(),
        "raw hook input must not be logged before an empty-allowlist denial"
    );

    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec!["/definitely/not-this-repository".to_string()]);
    });

    let output = repo
        .git_ai(&["checkpoint", "mock_ai", "private.txt"])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("excluded or not in the allowed_repositories"));
    assert!(
        !checkpoint_debug_log_dir(&repo).exists(),
        "raw hook input must not be logged for a denied repository"
    );

    repo.allow_only_self_for_collection();
    let nested = repo.path().join("malformed");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join(".git"), "not a gitdir pointer\n").unwrap();
    std::fs::write(nested.join("private.txt"), "sensitive input\n").unwrap();

    let output = repo
        .git_ai(&["checkpoint", "mock_ai", "malformed/private.txt"])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("repository authorization could not be verified"));
    assert!(
        !checkpoint_debug_log_dir(&repo).exists(),
        "raw hook input must not be logged when repository discovery fails"
    );
}

#[test]
fn test_checkpoint_debug_log_does_not_persist_zero_event_input() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let hook_input = serde_json::json!({
        "hookName": "TaskComplete",
        "clineVersion": "3.0.0",
        "taskId": "sensitive-zero-event-input",
        "workspaceRoots": [repo.canonical_path()],
    })
    .to_string();

    repo.git_ai(&["checkpoint", "cline", "--hook-input", &hook_input, "--"])
        .expect("a skipped preset event should preserve the hook exit-zero contract");

    assert!(
        !checkpoint_debug_log_dir(&repo).exists(),
        "zero parsed events prove no repository and must not persist raw hook input"
    );
}

#[test]
fn test_checkpoint_debug_log_does_not_persist_empty_file_event_input() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.allow_only_self_for_collection();
    repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let hook_input = serde_json::json!({
        "cwd": repo.canonical_path(),
        "file_paths": [],
    })
    .to_string();

    let output = repo
        .git_ai(&["checkpoint", "mock_ai", "--hook-input", &hook_input, "--"])
        .expect("an empty file event should preserve the hook exit-zero contract");

    assert!(
        output.trim().is_empty(),
        "an empty file event should be a silent no-op: {output}"
    );
    assert!(
        !checkpoint_debug_log_dir(&repo).exists(),
        "a file event without a repository-proving file must not persist raw hook input"
    );
}

#[test]
fn test_checkpoint_multi_repo_authorization_is_all_or_nothing() {
    let mut allowed_repo = TestRepo::new_dedicated_daemon();
    allowed_repo.allow_only_self_for_collection();
    allowed_repo.patch_git_ai_config(|patch| {
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let denied_repo = TestRepo::new();
    let allowed_file = allowed_repo.path().join("allowed.txt");
    let denied_file = denied_repo.path().join("denied.txt");
    std::fs::write(&allowed_file, "allowed repository input\n").unwrap();
    std::fs::write(&denied_file, "denied repository input\n").unwrap();

    let output = allowed_repo
        .git_ai(&[
            "checkpoint",
            "mock_ai",
            allowed_file.to_str().unwrap(),
            denied_file.to_str().unwrap(),
        ])
        .expect("a batch authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("excluded or not in the allowed_repositories"));
    assert!(
        !checkpoint_debug_log_dir(&allowed_repo).exists(),
        "no event in a mixed-repository batch may execute or log before all are authorized"
    );
    assert!(
        allowed_repo
            .current_working_logs()
            .read_all_checkpoints()
            .unwrap()
            .is_empty(),
        "the allowed prefix of a denied batch must not reach request persistence"
    );
}
