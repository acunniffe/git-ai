use super::*;
use crate::model::checkpoint_request::{
    BaseCommit, CheckpointFile, CheckpointRequest, PreparedPathRole,
};
use crate::model::working_log::CheckpointKind;
use crate::operations::daemon::actor_coordinator_side_effects::commit_enrichment_unrecoverable_error;
use crate::operations::daemon::cherry_pick_helpers::{
    rebase_new_tip_from_command, revert_source_args_for_side_effect,
};
use crate::operations::daemon::revert_rebase_helpers::strict_rebase_original_head_from_command;
use crate::operations::daemon::side_effect_helpers::commit_parent_pairs_from_log;
use serial_test::serial;
use std::collections::HashMap;
use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

struct EnvVarGuard {
    key: &'static str,
    original: Option<OsString>,
}

impl EnvVarGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: these tests are serialized via #[serial], so mutating the
        // process environment is isolated for the duration of each test.
        unsafe {
            std::env::set_var(key, value);
        }
        Self { key, original }
    }

    fn unset(key: &'static str) -> Self {
        let original = std::env::var_os(key);
        // SAFETY: these tests are serialized via #[serial], so mutating the
        // process environment is isolated for the duration of each test.
        unsafe {
            std::env::remove_var(key);
        }
        Self { key, original }
    }
}

impl Drop for EnvVarGuard {
    fn drop(&mut self) {
        match &self.original {
            Some(value) => {
                // SAFETY: these tests are serialized via #[serial], so restoring
                // process environment state is isolated for the duration of each test.
                unsafe {
                    std::env::set_var(self.key, value);
                }
            }
            None => {
                // SAFETY: these tests are serialized via #[serial], so restoring
                // process environment state is isolated for the duration of each test.
                unsafe {
                    std::env::remove_var(self.key);
                }
            }
        }
    }
}

fn sample_checkpoint_request() -> ControlRequest {
    ControlRequest::CheckpointRun {
        request: Box::new(CheckpointRequest {
            trace_id: "test-trace".to_string(),
            checkpoint_kind: CheckpointKind::Human,
            agent_id: None,
            files: vec![CheckpointFile {
                path: std::path::PathBuf::from("test.txt"),
                content: None,
                repo_work_dir: std::path::PathBuf::from("/tmp/repo"),
                base_commit: BaseCommit::Initial,
            }],
            path_role: PreparedPathRole::WillEdit,
            stream_source: None,
            metadata: std::collections::HashMap::new(),
        }),
    }
}

#[tokio::test]
async fn checkpoint_ingress_methods_apply_the_same_request_bounds() {
    let repo_work_dir = std::env::temp_dir().join("git-ai-checkpoint-bounds");
    let file = CheckpointFile {
        path: repo_work_dir.join("file.rs"),
        content: None,
        repo_work_dir,
        base_commit: BaseCommit::Initial,
    };
    let request = CheckpointRequest {
        trace_id: "oversized-checkpoint".to_string(),
        checkpoint_kind: CheckpointKind::Human,
        agent_id: None,
        files: vec![file; crate::model::checkpoint_delivery::CHECKPOINT_DELIVERY_MAX_FILES + 1],
        path_role: PreparedPathRole::Edited,
        stream_source: None,
        metadata: HashMap::new(),
    };
    let delivery = crate::model::checkpoint_delivery::CheckpointDelivery::from_requests_at(
        vec![request.clone()],
        42,
    )
    .remove(0);
    let coordinator = ActorDaemonCoordinator::new();

    let legacy = coordinator
        .handle_control_request(ControlRequest::CheckpointRun {
            request: Box::new(request),
        })
        .await;
    let durable = coordinator
        .handle_control_request(ControlRequest::CheckpointDeliver {
            delivery: Box::new(delivery),
        })
        .await;

    let expected = "checkpoint delivery request.files exceeds limit 1000 (actual 1001)";
    assert!(!legacy.ok);
    assert!(!durable.ok);
    assert_eq!(legacy.error.as_deref(), Some(expected));
    assert_eq!(legacy.error, durable.error);
}

fn run_git_for_test(repo: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .unwrap_or_else(|error| panic!("git {:?} failed to spawn: {}", args, error));
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git stdout should be utf8")
        .trim()
        .to_string()
}

fn run_git_stdin_for_test(repo: &std::path::Path, args: &[&str], stdin: &str) -> String {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("git {:?} failed to spawn: {}", args, error));
    child
        .stdin
        .take()
        .expect("stdin should be piped")
        .write_all(stdin.as_bytes())
        .expect("write git stdin");
    let output = child
        .wait_with_output()
        .unwrap_or_else(|error| panic!("git {:?} failed to wait: {}", args, error));
    assert!(
        output.status.success(),
        "git {:?} failed\nstdout: {}\nstderr: {}",
        args,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("git stdout should be utf8")
        .trim()
        .to_string()
}

fn test_rebase_command(
    invoked_args: &[&str],
    ref_changes: Vec<crate::model::domain::RefChange>,
) -> crate::model::domain::NormalizedCommand {
    crate::model::domain::NormalizedCommand {
        scope: crate::model::domain::CommandScope::Family(crate::model::domain::FamilyKey(
            "/repo/.git".to_string(),
        )),
        family_key: Some(crate::model::domain::FamilyKey("/repo/.git".to_string())),
        worktree: Some(PathBuf::from("/repo")),
        root_sid: "rebase-test".to_string(),
        trace_derived: false,
        raw_argv: std::iter::once("git")
            .chain(std::iter::once("rebase"))
            .chain(invoked_args.iter().copied())
            .map(str::to_string)
            .collect(),
        primary_command: Some("rebase".to_string()),
        invoked_command: Some("rebase".to_string()),
        invoked_args: invoked_args.iter().map(|arg| (*arg).to_string()).collect(),
        observed_child_commands: Vec::new(),
        exit_code: 0,
        started_at_ns: 1,
        finished_at_ns: 2,
        reflog_start_offsets: HashMap::new(),
        stash_target_oid: None,
        cherry_pick_source_oids: Vec::new(),
        revert_source_oids: Vec::new(),
        ref_changes,
        confidence: crate::model::domain::Confidence::High,
    }
}

fn ref_change(reference: &str, old: &str, new: &str) -> crate::model::domain::RefChange {
    crate::model::domain::RefChange {
        reference: reference.to_string(),
        old: old.to_string(),
        new: new.to_string(),
    }
}

#[test]
fn revert_side_effect_source_named_like_command_is_not_dropped() {
    let mut command = test_rebase_command(&[], Vec::new());
    command.raw_argv = vec![
        "git".to_string(),
        "revert".to_string(),
        "revert".to_string(),
    ];
    command.primary_command = Some("revert".to_string());
    command.invoked_command = Some("revert".to_string());
    command.invoked_args = vec!["revert".to_string()];

    assert_eq!(
        revert_source_args_for_side_effect(&command),
        vec!["revert".to_string()]
    );
}

#[test]
fn parent_diff_batch_ignores_root_and_missing_parent_log_entries() {
    assert_eq!(
        commit_parent_pairs_from_log("root\nchild root\nmissing-parent\nmerge first second\n"),
        vec![
            ("child".to_string(), "root".to_string()),
            ("merge".to_string(), "first".to_string()),
        ]
    );
}

#[test]
#[serial]
fn conflict_resolution_note_read_errors_are_not_silently_ignored() {
    let temp = tempfile::tempdir().unwrap();
    let repo_path = temp.path().join("repo");
    std::fs::create_dir_all(&repo_path).unwrap();
    run_git_for_test(&repo_path, &["init"]);
    run_git_for_test(&repo_path, &["config", "user.name", "Test User"]);
    run_git_for_test(&repo_path, &["config", "user.email", "test@example.com"]);

    std::fs::write(repo_path.join("file.txt"), "onto\n").unwrap();
    run_git_for_test(&repo_path, &["add", "file.txt"]);
    run_git_for_test(&repo_path, &["commit", "-m", "onto"]);
    let onto = run_git_for_test(&repo_path, &["rev-parse", "HEAD"]);

    std::fs::write(repo_path.join("file.txt"), "onto\nnew\n").unwrap();
    run_git_for_test(&repo_path, &["add", "file.txt"]);
    run_git_for_test(&repo_path, &["commit", "-m", "new"]);
    let new_tip = run_git_for_test(&repo_path, &["rev-parse", "HEAD"]);

    let missing_blob = "2222222222222222222222222222222222222222";
    let prefix = &new_tip[..2];
    let suffix = &new_tip[2..];
    let leaf_tree = run_git_stdin_for_test(
        &repo_path,
        &["mktree", "--missing"],
        &format!("100644 blob {missing_blob}\t{suffix}\n"),
    );
    let root_tree = run_git_stdin_for_test(
        &repo_path,
        &["mktree"],
        &format!("040000 tree {leaf_tree}\t{prefix}\n"),
    );
    run_git_for_test(&repo_path, &["update-ref", "refs/notes/ai", &root_tree]);

    let repo = crate::operations::git::find_repository_in_path(repo_path.to_str().unwrap())
        .expect("find test repository");
    let result = process_conflict_resolution_working_logs(&repo, &new_tip, Some(&onto));
    assert!(
        result.is_err(),
        "corrupt destination notes must fail closed instead of being treated as absent"
    );
}

#[test]
fn checkpoint_requests_use_long_timeout_in_ci_or_test_env() {
    assert_eq!(
        checkpoint_control_response_timeout(&sample_checkpoint_request(), true),
        DAEMON_CHECKPOINT_RESPONSE_TIMEOUT
    );
}

#[test]
fn checkpoint_requests_use_short_timeout_in_product_env() {
    assert_eq!(
        checkpoint_control_response_timeout(&sample_checkpoint_request(), false),
        DAEMON_CONTROL_RESPONSE_TIMEOUT
    );
}

#[test]
fn transcript_sweep_triggers_for_commit_amend_and_push_events() {
    use crate::model::domain::SemanticEvent;
    use crate::operations::daemon::stream_worker::SweepTrigger;

    assert_eq!(
        transcript_sweep_triggers_for_events(&[SemanticEvent::CommitCreated {
            base: Some("base".to_string()),
            new_head: "new".to_string(),
        }]),
        vec![SweepTrigger::PostCommit]
    );
    assert_eq!(
        transcript_sweep_triggers_for_events(&[SemanticEvent::CommitAmended {
            old_head: "old".to_string(),
            new_head: "new".to_string(),
        }]),
        vec![SweepTrigger::PostCommit]
    );
    assert_eq!(
        transcript_sweep_triggers_for_events(&[SemanticEvent::PushCompleted {
            remote: Some("origin".to_string()),
        }]),
        vec![SweepTrigger::PostPush]
    );
    assert_eq!(
        transcript_sweep_triggers_for_events(&[
            SemanticEvent::CommitCreated {
                base: Some("base".to_string()),
                new_head: "new".to_string(),
            },
            SemanticEvent::PushCompleted {
                remote: Some("origin".to_string()),
            },
        ]),
        vec![SweepTrigger::PostCommit, SweepTrigger::PostPush]
    );
}

#[test]
fn explicit_branch_rebase_original_head_prefers_branch_ref_over_head() {
    const MAIN: &str = "1111111111111111111111111111111111111111";
    const FEATURE: &str = "2222222222222222222222222222222222222222";
    const ONTO: &str = "3333333333333333333333333333333333333333";

    let cmd = test_rebase_command(
        &["master", "scenario-3-multi-file-conflict"],
        vec![
            ref_change("HEAD", MAIN, FEATURE),
            ref_change("HEAD", FEATURE, ONTO),
            ref_change(
                "refs/heads/scenario-3-multi-file-conflict",
                FEATURE,
                FEATURE,
            ),
        ],
    );

    assert_eq!(
        strict_rebase_original_head_from_command(&cmd, MAIN),
        Some(FEATURE.to_string()),
        "explicit branch rebase must store the target branch tip, not the caller's original HEAD"
    );
}

#[test]
fn pending_rebase_new_tip_prefers_matching_branch_ref_over_later_head_noise() {
    const ORIGINAL: &str = "1111111111111111111111111111111111111111";
    const ONTO: &str = "2222222222222222222222222222222222222222";
    const NEW_TIP: &str = "3333333333333333333333333333333333333333";
    const UNRELATED_HEAD: &str = "4444444444444444444444444444444444444444";

    let cmd = test_rebase_command(
        &["--continue"],
        vec![
            ref_change("HEAD", ONTO, NEW_TIP),
            ref_change(
                "refs/heads/scenario-3-multi-file-conflict",
                ORIGINAL,
                NEW_TIP,
            ),
            ref_change("HEAD", NEW_TIP, UNRELATED_HEAD),
        ],
    );

    assert_eq!(
        rebase_new_tip_from_command(&cmd, ORIGINAL),
        Some(NEW_TIP.to_string()),
        "pending rebase completion must use the branch ref update that rewrote the original tip"
    );
}

#[test]
#[serial]
fn checkpoint_control_timeout_uses_ci_env_var() {
    let _unset_test = EnvVarGuard::unset("GIT_AI_TEST_DB_PATH");
    let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");
    let _set_ci = EnvVarGuard::set("CI", "true");

    assert!(checkpoint_control_timeout_uses_ci_or_test_budget());
}

#[test]
#[serial]
fn checkpoint_control_timeout_uses_test_db_env_var() {
    let _unset_ci = EnvVarGuard::unset("CI");
    let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");
    let _set_test = EnvVarGuard::set("GIT_AI_TEST_DB_PATH", "/tmp/git-ai-test.db");

    assert!(checkpoint_control_timeout_uses_ci_or_test_budget());
}

#[test]
#[serial]
fn checkpoint_control_timeout_false_when_no_ci_or_test_vars() {
    let _unset_ci = EnvVarGuard::unset("CI");
    let _unset_test = EnvVarGuard::unset("GIT_AI_TEST_DB_PATH");
    let _unset_legacy_test = EnvVarGuard::unset("GITAI_TEST_DB_PATH");

    assert!(!checkpoint_control_timeout_uses_ci_or_test_budget());
}

#[test]
fn compute_watermarks_uses_symlink_metadata_not_target_mtime() {
    // Verify that compute_watermarks_from_stat uses lstat (symlink's own mtime)
    // not stat (target file's mtime), consistent with snapshot's symlink_metadata.
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();

    // Create a target file
    let target = dir.join("target.txt");
    std::fs::write(&target, b"hello").unwrap();

    // Create a symlink pointing to the target
    let link = dir.join("link.txt");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &link).unwrap();
    #[cfg(windows)]
    std::os::windows::fs::symlink_file(&target, &link).unwrap();

    // Watermark the symlink
    let wm = compute_watermarks_from_stat(dir.to_str().unwrap(), &["link.txt".to_string()]);

    // The watermark should match symlink_metadata mtime, not target metadata mtime.
    let symlink_meta = std::fs::symlink_metadata(&link).unwrap();
    let target_meta = std::fs::metadata(&link).unwrap(); // follows symlink

    let symlink_mtime = symlink_meta
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let target_mtime = target_meta
        .modified()
        .unwrap()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_nanos();

    let recorded = *wm.get("link.txt").unwrap();

    assert_eq!(
        recorded, symlink_mtime,
        "watermark should match lstat mtime of the symlink itself"
    );
    // This assertion documents the intent: if symlink and target mtimes differ,
    // the watermark must track the symlink, not the target.
    let _ = target_mtime; // used only as documentation; may equal symlink_mtime on some FS
}

fn test_commit_command(
    invoked_args: &[&str],
    exit_code: i32,
) -> crate::model::domain::NormalizedCommand {
    let mut cmd = test_rebase_command(invoked_args, Vec::new());
    cmd.raw_argv = std::iter::once("git".to_string())
        .chain(std::iter::once("commit".to_string()))
        .chain(invoked_args.iter().map(|arg| arg.to_string()))
        .collect();
    cmd.primary_command = Some("commit".to_string());
    cmd.invoked_command = Some("commit".to_string());
    cmd.exit_code = exit_code;
    cmd
}

#[test]
fn commit_enrichment_unrecoverable_error_fires_for_opaque_exit_zero_commit() {
    let cmd = test_commit_command(&["-m", "msg"], 0);
    let error = commit_enrichment_unrecoverable_error(
        &cmd,
        &[crate::model::domain::SemanticEvent::OpaqueCommand],
    );
    assert!(
        error.is_some(),
        "an exit-0 commit whose only event is OpaqueCommand must fail loud"
    );
}

#[test]
fn commit_enrichment_unrecoverable_error_is_none_for_dry_run() {
    let cmd = test_commit_command(&["-m", "msg", "--dry-run"], 0);
    assert!(
        commit_enrichment_unrecoverable_error(
            &cmd,
            &[crate::model::domain::SemanticEvent::OpaqueCommand],
        )
        .is_none(),
        "--dry-run never touches the reflog and must not be treated as a lost note"
    );
}

#[test]
fn commit_enrichment_unrecoverable_error_is_none_for_dry_run_synonyms() {
    // Per git-commit(1), --short / --porcelain / --long each imply --dry-run
    // and exit 0 with staged changes while moving no ref.
    for flag in ["--short", "--porcelain", "--long"] {
        let cmd = test_commit_command(&["-m", "msg", flag], 0);
        assert!(
            commit_enrichment_unrecoverable_error(
                &cmd,
                &[crate::model::domain::SemanticEvent::OpaqueCommand],
            )
            .is_none(),
            "{flag} implies --dry-run for git commit and must not fire the gate"
        );
    }
}

#[test]
fn commit_enrichment_unrecoverable_error_is_none_for_nonzero_exit() {
    let cmd = test_commit_command(&["-m", "msg"], 1);
    assert!(
        commit_enrichment_unrecoverable_error(
            &cmd,
            &[crate::model::domain::SemanticEvent::OpaqueCommand],
        )
        .is_none(),
        "a rejected/failed commit attempt routinely moves no ref and is not a bug"
    );
}

#[test]
fn commit_enrichment_unrecoverable_error_is_none_when_head_transition_found() {
    let cmd = test_commit_command(&["-m", "msg"], 0);
    let events = [crate::model::domain::SemanticEvent::CommitCreated {
        base: None,
        new_head: "head1".to_string(),
    }];
    assert!(
        commit_enrichment_unrecoverable_error(&cmd, &events).is_none(),
        "a commit that resolved a HEAD transition must never be flagged"
    );
}

#[test]
fn commit_enrichment_unrecoverable_error_is_none_for_non_commit_commands() {
    let mut cmd = test_commit_command(&[], 0);
    cmd.primary_command = Some("branch".to_string());
    assert!(
        commit_enrichment_unrecoverable_error(
            &cmd,
            &[crate::model::domain::SemanticEvent::OpaqueCommand],
        )
        .is_none(),
        "an opaque outcome for a non-commit command (e.g. `git branch`) is routine"
    );
}
