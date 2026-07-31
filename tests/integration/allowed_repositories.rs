//! Collection is opt-in: with an empty `allowed_repositories` list git-ai
//! collects nothing. TestRepo's default config patch allows the OS temp root,
//! so these tests override the allowlist explicitly where needed. Denied-repo
//! tests use dedicated daemons: the shared daemon's home config must not be
//! rewritten with an empty allowlist while other tests run against it.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use git_ai::model::checkpoint_delivery::CHECKPOINT_DELIVERY_MAX_FILES;
use std::fs;
use std::io::Write;
use std::process::Stdio;

#[cfg(unix)]
fn artifact_tree_contains_bytes(root: &std::path::Path, needle: &[u8]) -> bool {
    let Ok(metadata) = fs::symlink_metadata(root) else {
        return false;
    };
    if metadata.file_type().is_symlink() {
        return false;
    }
    if metadata.is_file() {
        return fs::read(root)
            .is_ok_and(|bytes| bytes.windows(needle.len()).any(|window| window == needle));
    }
    fs::read_dir(root).is_ok_and(|entries| {
        entries
            .filter_map(Result::ok)
            .any(|entry| artifact_tree_contains_bytes(&entry.path(), needle))
    })
}

#[test]
fn test_checkpoint_denied_with_empty_allowlist() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![]);
    });

    fs::write(repo.path().join("file.txt"), "AI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"])
        .expect("denied checkpoint should exit successfully as a no-op");

    let working_logs = repo.path().join(".git/ai/working_logs");
    let entries: Vec<_> = match fs::read_dir(&working_logs) {
        Ok(dir) => dir.filter_map(Result::ok).map(|e| e.path()).collect(),
        Err(_) => vec![],
    };
    assert!(
        entries.is_empty(),
        "a denied repository must not get working log entries, found: {entries:?}"
    );
}

#[test]
fn test_excluded_repository_authorization_does_not_create_git_ai_storage() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let excluded_repo = repo.path().join("excluded-repository");
    fs::create_dir_all(&excluded_repo).unwrap();
    repo.git_og(&["-C", "excluded-repository", "init", "--quiet"])
        .expect("nested excluded repository should initialize");
    fs::write(excluded_repo.join("private.txt"), "sensitive input\n").unwrap();
    let excluded_ai_dir = excluded_repo.join(".git/ai");
    assert!(
        !excluded_ai_dir.exists(),
        "the fixture must begin without git-ai repository storage"
    );
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec!["/definitely/not-this-repository".to_string()]);
    });

    let output = repo
        .git_ai_from_working_dir(&excluded_repo, &["checkpoint", "mock_ai", "private.txt"])
        .expect("an authorization denial should preserve the hook exit-zero contract");

    assert!(output.contains("excluded or not in the allowed_repositories"));
    assert!(
        !excluded_ai_dir.exists(),
        "read-only authorization must not create .git/ai storage in an excluded repository"
    );
}

#[test]
fn test_file_checkpoint_authorizes_absolute_file_without_authorizing_cwd() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.allow_only_self_for_collection();
    let file_path = repo.path().join("allowed.txt");
    fs::write(&file_path, "allowed repository input\n").unwrap();
    let unrelated_cwd = repo.test_home_path().join("unrelated-cwd");
    fs::create_dir_all(&unrelated_cwd).unwrap();

    let output = repo
        .git_ai_from_working_dir(
            &unrelated_cwd,
            &[
                "checkpoint",
                "mock_ai",
                file_path.to_str().expect("test path should be UTF-8"),
            ],
        )
        .expect("an absolute file in an allowed repository should be checkpointed");

    assert!(
        output.contains("checkpoint_requests=1"),
        "the file repository, not the unrelated hook CWD, should anchor authorization: {output}"
    );
}

#[cfg(unix)]
#[test]
fn test_symlinked_denied_file_is_rejected_before_any_checkpoint_side_effect() {
    use std::os::unix::fs::symlink;

    let mut allowed_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let allowed_root = allowed_repo.canonical_path().to_string_lossy().to_string();
    allowed_repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![allowed_root]);
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let denied_repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let denied_secret = "SYMLINK-DENIED-REPOSITORY-SECRET-7f39c18a\n";
    let denied_file = denied_repo.path().join("private.txt");
    fs::write(&denied_file, denied_secret).unwrap();

    let allowed_file = allowed_repo.path().join("allowed.txt");
    fs::write(&allowed_file, "allowed prefix must not execute\n").unwrap();
    let denied_link = allowed_repo.path().join("denied-secret-link.txt");
    symlink(&denied_file, &denied_link).unwrap();
    let outbox = allowed_repo
        .test_home_path()
        .join("managed-checkpoint-outbox");
    let mut command = allowed_repo.git_ai_command_without_pre_sync_for_test(
        &[
            "checkpoint",
            "mock_ai",
            allowed_file.to_str().unwrap(),
            denied_link.to_str().unwrap(),
        ],
        &[("GIT_AI_CHECKPOINT_OUTBOX_DIR", outbox.to_str().unwrap())],
    );
    let output = command.output().expect("symlink checkpoint should run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("excluded or not in the allowed_repositories"),
        "the symlink target repository must determine authorization: {combined}"
    );
    assert!(
        !combined.contains(denied_secret.trim()) && !combined.contains("private.txt"),
        "authorization diagnostics must not disclose the denied target: {combined}"
    );
    assert!(
        !allowed_repo.daemon_control_socket_path().exists(),
        "authorization must finish before daemon or live delivery"
    );
    assert!(
        !allowed_repo
            .test_home_path()
            .join(".git-ai/internal/checkpoint-debug-logs")
            .exists(),
        "a denied batch must not persist raw hook input in the debug log"
    );
    assert!(
        !outbox.exists()
            || fs::read_dir(&outbox)
                .unwrap()
                .filter_map(Result::ok)
                .next()
                .is_none(),
        "a denied batch must not publish an outbox record"
    );

    for artifact_root in [
        allowed_repo.path().join(".git/ai"),
        allowed_repo.test_home_path().join(".git-ai"),
        allowed_repo.test_db_path().clone(),
        outbox,
    ] {
        assert!(
            !artifact_tree_contains_bytes(&artifact_root, denied_secret.as_bytes()),
            "denied file bytes leaked into checkpoint artifacts under {}",
            artifact_root.display()
        );
    }
}

#[test]
fn test_checkpoint_above_file_limit_denies_whole_batch_before_authorization() {
    let mut repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let allowed_root = repo.canonical_path().to_string_lossy().to_string();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![allowed_root]);
        patch.feature_flags = Some(serde_json::json!({"checkpoint_debug_log": true}));
    });
    let file_path = repo.path().join("bounded.txt");
    fs::write(&file_path, "must not be checkpointed\n").unwrap();
    let hook_input = serde_json::json!({
        "cwd": repo.canonical_path(),
        "file_paths": vec![
            file_path.to_string_lossy().to_string();
            CHECKPOINT_DELIVERY_MAX_FILES + 1
        ],
    })
    .to_string();
    let outbox = repo.test_home_path().join("bounded-checkpoint-outbox");
    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "mock_ai", "--hook-input", "stdin", "--"],
        &[("GIT_AI_CHECKPOINT_OUTBOX_DIR", outbox.to_str().unwrap())],
    );
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .expect("bounded checkpoint process should start");
    child
        .stdin
        .take()
        .expect("bounded checkpoint stdin should be piped")
        .write_all(hook_input.as_bytes())
        .expect("bounded checkpoint hook input should be written");
    let output = child
        .wait_with_output()
        .expect("bounded checkpoint should run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("checkpoint file count exceeds the supported limit"),
        "the whole oversized batch must be denied before authorizing a prefix: {combined}"
    );
    assert!(!repo.daemon_control_socket_path().exists());
    assert!(
        !repo
            .test_home_path()
            .join(".git-ai/internal/checkpoint-debug-logs")
            .exists()
    );
    assert!(!repo.path().join(".git/ai").exists());
    assert!(
        !outbox.exists()
            || fs::read_dir(&outbox)
                .unwrap()
                .filter_map(Result::ok)
                .next()
                .is_none()
    );
}

#[test]
fn test_denied_no_arg_checkpoint_does_not_discover_dirty_files() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec!["/definitely/not-this-repository".to_string()]);
    });
    let excluded_repo = repo.path().join("excluded-no-arg-repository");
    fs::create_dir_all(&excluded_repo).unwrap();
    repo.git_og(&["-C", "excluded-no-arg-repository", "init", "--quiet"])
        .expect("nested excluded repository should initialize");
    fs::write(excluded_repo.join("private.txt"), "sensitive input\n").unwrap();
    let excluded_ai_dir = excluded_repo.join(".git/ai");
    let spawn_log = repo.test_home_path().join("denied-no-arg-spawns.log");

    let mut command = repo.git_ai_command_without_pre_sync_for_test(
        &["checkpoint", "mock_ai"],
        &[(
            "GIT_AI_SPAWN_LOG",
            spawn_log.to_str().expect("test path should be UTF-8"),
        )],
    );
    let output = command
        .current_dir(&excluded_repo)
        .output()
        .expect("denied checkpoint should run");
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(output.status.success(), "{combined}");
    assert!(
        combined.contains("excluded or not in the allowed_repositories"),
        "{combined}"
    );
    let spawns = fs::read_to_string(&spawn_log).unwrap_or_default();
    assert!(
        !spawns.lines().any(|command| command == "status"),
        "authorization must precede dirty-status discovery; spawns: {spawns:?}"
    );
    assert!(
        !excluded_ai_dir.exists(),
        "authorization must precede repository storage construction"
    );
}

#[test]
fn test_commit_in_denied_repo_writes_no_authorship_note() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![]);
    });

    fs::write(repo.path().join("file.txt"), "AI line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "file.txt"])
        .expect("denied checkpoint should exit successfully as a no-op");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "commit in denied repo"])
        .unwrap();
    repo.sync_daemon();

    let head = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(
        repo.read_authorship_note(&head).is_none(),
        "a denied repository must not get authorship notes"
    );
}

#[test]
fn test_default_test_allowlist_allows_collection_via_path() {
    // TestRepo repos have no remotes; collection works because the default
    // test allowlist contains the OS temp root as a path entry.
    let repo = TestRepo::new();
    let mut file = repo.filename("test.txt");
    file.set_contents(lines!["Human line", "AI line".ai()]);
    repo.stage_all_and_commit("Initial commit").unwrap();
    file.assert_lines_and_blame(lines!["Human line".human(), "AI line".ai()]);
}

#[test]
fn test_shared_daemon_config_isolated_from_custom_fixture() {
    let repo = TestRepo::new();
    let mut config_mutator = TestRepo::new();
    config_mutator.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![]);
    });

    let mut file = repo.filename("shared-config.txt");
    file.set_contents(lines!["AI line".ai()]);
    repo.stage_all_and_commit("shared daemon configuration isolation")
        .expect("another fixture's config must not disable collection");
    file.assert_committed_lines(lines!["AI line".ai()]);
}

crate::reuse_tests_in_worktree!(test_shared_daemon_config_isolated_from_custom_fixture,);

#[test]
fn test_reallowing_repo_restores_collection() {
    let mut repo = TestRepo::new_dedicated_daemon();
    repo.patch_git_ai_config(|patch| {
        patch.allowed_repositories = Some(vec![]);
    });

    let file_path = repo.path().join("example.txt");
    fs::write(&file_path, "Untracked line\n").unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .expect("denied checkpoint should exit successfully as a no-op");
    repo.git(&["add", "-A"]).unwrap();
    repo.git(&["commit", "-m", "denied commit"]).unwrap();
    repo.sync_daemon();
    let first = repo.git(&["rev-parse", "HEAD"]).unwrap().trim().to_string();
    assert!(repo.read_authorship_note(&first).is_none());

    // Allow this repository by its root path and verify collection resumes.
    repo.allow_only_self_for_collection();

    let second_edit = "\
Untracked line
AI line
";
    fs::write(&file_path, second_edit).unwrap();
    repo.git_ai(&["checkpoint", "mock_ai", "example.txt"])
        .expect("checkpoint should succeed once the repo is allowed");
    repo.stage_all_and_commit("allowed commit").unwrap();

    let mut file = repo.filename("example.txt");
    file.assert_committed_lines(lines![
        "Untracked line".unattributed_human(),
        "AI line".ai(),
    ]);
}

#[test]
fn test_config_cli_accepts_canonical_and_legacy_allowlist_keys() {
    let repo = TestRepo::new();
    // Entries are validated: a path must point at an existing git repository.
    let repo_root = repo.canonical_path().to_string_lossy().replace('\\', "/");
    repo.git_ai(&["config", "--add", "allowed_repositories", &repo_root])
        .expect("adding an allowlist entry should succeed");

    // Read without the pre-invocation config sync: the sync rewrites
    // config.json from the test patch and would clobber the entry just added.
    let canonical = repo
        .git_ai_without_pre_sync_for_test(&["config", "allowed_repositories"])
        .expect("canonical key should be readable");
    assert!(
        canonical.contains(&repo_root),
        "expected added entry in: {canonical}"
    );

    let legacy = repo
        .git_ai_without_pre_sync_for_test(&["config", "allow_repositories"])
        .expect("legacy key should remain readable");
    assert!(
        legacy.contains(&repo_root),
        "expected added entry via legacy key in: {legacy}"
    );
}
