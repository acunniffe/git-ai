use std::fs;
use std::sync::Arc;

use crate::model::domain::CommandScope;

use super::TraceNormalizer;
use super::tests_lifecycle::{MockBackend, atexit_payload};

#[test]
fn normalizer_defers_exit_seen_before_start_until_atexit() {
    let backend = Arc::new(MockBackend::default());
    backend.set_family("/repo", "/repo/.git");
    let mut normalizer = TraceNormalizer::new(backend);

    let exit = serde_json::json!({
        "event":"exit",
        "sid":"s2",
        "ts":10,
        "code":0
    });
    let start = serde_json::json!({
        "event":"start",
        "sid":"s2",
        "ts":1,
        "argv":["git","status"],
        "worktree":"/repo"
    });
    let atexit = serde_json::json!({
        "event":"atexit",
        "sid":"s2",
        "ts":11,
        "code":0
    });

    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
    assert_eq!(cmd.root_sid, "s2");
    assert_eq!(cmd.primary_command.as_deref(), Some("status"));
    assert_eq!(cmd.exit_code, 0);
}

#[test]
fn child_cmd_name_enriches_root() {
    let backend = Arc::new(MockBackend::default());
    backend.set_family("/repo", "/repo/.git");
    let mut normalizer = TraceNormalizer::new(backend);

    let start = serde_json::json!({
        "event":"start",
        "sid":"s3",
        "ts":1,
        "argv":["git","foo"],
        "worktree":"/repo"
    });
    let child = serde_json::json!({
        "event":"cmd_name",
        "sid":"s3/child1",
        "ts":2,
        "name":"status"
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"s3",
        "ts":3,
        "code":0
    });
    let atexit = atexit_payload("s3", 4);

    normalizer.ingest_payload(&start).unwrap();
    normalizer.ingest_payload(&child).unwrap();
    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
    assert_eq!(cmd.observed_child_commands, vec!["status".to_string()]);
    assert_eq!(cmd.primary_command.as_deref(), Some("status"));
}

#[test]
fn child_exit_does_not_finalize_without_root_exit() {
    let backend = Arc::new(MockBackend::default());
    backend.set_family("/repo", "/repo/.git");
    let mut normalizer = TraceNormalizer::new(backend);

    let start = serde_json::json!({
        "event":"start",
        "sid":"s-exec",
        "ts":1,
        "argv":["git","notes","show","abc123"],
        "worktree":"/repo"
    });
    let cmd_name = serde_json::json!({
        "event":"cmd_name",
        "sid":"s-exec",
        "ts":2,
        "name":"notes"
    });
    let exec = serde_json::json!({
        "event":"exec",
        "sid":"s-exec",
        "ts":3,
        "argv":["git","show","def456"]
    });
    let child_exit = serde_json::json!({
        "event":"exit",
        "sid":"s-exec/child",
        "ts":4,
        "code":0
    });
    let root_exit = serde_json::json!({
        "event":"exit",
        "sid":"s-exec",
        "ts":5,
        "code":0
    });
    let root_atexit = atexit_payload("s-exec", 6);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());
    assert!(normalizer.ingest_payload(&exec).unwrap().is_none());
    assert!(normalizer.ingest_payload(&child_exit).unwrap().is_none());
    assert_eq!(normalizer.state().pending.len(), 1);

    assert!(normalizer.ingest_payload(&root_exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&root_atexit).unwrap().unwrap();
    assert_eq!(cmd.root_sid, "s-exec");
    assert_eq!(cmd.primary_command.as_deref(), Some("notes"));
    assert_eq!(cmd.exit_code, 0);
    assert!(normalizer.state().pending.is_empty());
}

#[test]
fn child_exit_before_root_exec_is_ignored_until_root_exit() {
    let backend = Arc::new(MockBackend::default());
    backend.set_family("/repo", "/repo/.git");
    let mut normalizer = TraceNormalizer::new(backend);

    let start = serde_json::json!({
        "event":"start",
        "sid":"s-exec-oop",
        "ts":1,
        "argv":["git","notes","show","abc123"],
        "worktree":"/repo"
    });
    let cmd_name = serde_json::json!({
        "event":"cmd_name",
        "sid":"s-exec-oop",
        "ts":2,
        "name":"notes"
    });
    let child_exit = serde_json::json!({
        "event":"exit",
        "sid":"s-exec-oop/child",
        "ts":3,
        "code":0
    });
    let exec = serde_json::json!({
        "event":"exec",
        "sid":"s-exec-oop",
        "ts":4,
        "argv":["git","show","def456"]
    });
    let root_exit = serde_json::json!({
        "event":"exit",
        "sid":"s-exec-oop",
        "ts":5,
        "code":0
    });
    let root_atexit = atexit_payload("s-exec-oop", 6);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());
    assert!(normalizer.ingest_payload(&child_exit).unwrap().is_none());
    assert_eq!(normalizer.state().pending.len(), 1);

    assert!(normalizer.ingest_payload(&exec).unwrap().is_none());
    assert!(normalizer.ingest_payload(&root_exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&root_atexit).unwrap().unwrap();
    assert_eq!(cmd.root_sid, "s-exec-oop");
    assert_eq!(cmd.primary_command.as_deref(), Some("notes"));
    assert_eq!(cmd.exit_code, 0);
    assert!(normalizer.state().pending.is_empty());
}

#[test]
fn clone_relative_target_falls_back_to_argv_target_when_def_repo_candidate_fails() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let outer = temp.path().join("outer");
    let clone_dir = outer.join("nested").join("relative-clone");
    crate::operations::git::test_utils::seed_valid_git_dir(&clone_dir.join(".git"));

    let start = serde_json::json!({
        "event":"start",
        "sid":"clone-rel",
        "ts":1,
        "argv":["git","clone","ssh://example/repo.git","nested/relative-clone"],
        "worktree":outer
    });
    let def_repo = serde_json::json!({
        "event":"def_repo",
        "sid":"clone-rel",
        "ts":2,
        "repo":clone_dir.join(".git")
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"clone-rel",
        "ts":3,
        "code":0
    });
    let atexit = atexit_payload("clone-rel", 4);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

    assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
    assert_eq!(cmd.worktree.as_ref(), Some(&clone_dir));
    assert!(matches!(cmd.scope, CommandScope::Family(_)));
}

#[test]
fn clone_with_late_family_resolution_does_not_need_ref_metadata() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let outer = temp.path().join("outer");
    let clone_dir = outer.join("nested").join("relative-clone");
    fs::create_dir_all(clone_dir.parent().expect("clone parent")).expect("create clone parent");

    let start = serde_json::json!({
        "event":"start",
        "sid":"clone-late-family",
        "ts":1,
        "argv":["git","clone","ssh://example/repo.git","nested/relative-clone"],
        "worktree":outer
    });
    let def_repo = serde_json::json!({
        "event":"def_repo",
        "sid":"clone-late-family",
        "ts":2,
        "worktree":clone_dir
    });
    let cmd_name = serde_json::json!({
        "event":"cmd_name",
        "sid":"clone-late-family",
        "ts":3,
        "name":"clone"
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"clone-late-family",
        "ts":4,
        "code":0
    });
    let atexit = atexit_payload("clone-late-family", 5);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
    assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());

    // Simulate repo discoverability only once clone is about to exit.
    crate::operations::git::test_utils::seed_valid_git_dir(&clone_dir.join(".git"));

    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer
        .ingest_payload(&atexit)
        .expect("clone finalize should not error")
        .expect("clone should emit a normalized command");

    assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
    assert!(matches!(cmd.scope, CommandScope::Family(_)));
}

#[test]
fn clone_prefers_target_family_over_source_cwd_family() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let source_repo = temp.path().join("source-repo");
    let cloned_repo = temp.path().join("cloned-repo");
    crate::operations::git::test_utils::seed_valid_git_dir(&source_repo.join(".git"));
    crate::operations::git::test_utils::seed_valid_git_dir(&cloned_repo.join(".git"));

    let start = serde_json::json!({
        "event":"start",
        "sid":"clone-source-cwd",
        "ts":1,
        "argv":["git","clone","ssh://example/repo.git",cloned_repo],
        "worktree":source_repo
    });
    let def_repo = serde_json::json!({
        "event":"def_repo",
        "sid":"clone-source-cwd",
        "ts":2,
        "repo":cloned_repo.join(".git")
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"clone-source-cwd",
        "ts":3,
        "code":0
    });
    let atexit = atexit_payload("clone-source-cwd", 4);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();

    assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
    assert_eq!(cmd.worktree.as_ref(), Some(&cloned_repo));
    let expected_family =
        crate::operations::git::canonicalize::canonicalize_or_self(&cloned_repo.join(".git"));
    assert_eq!(
        cmd.family_key.as_ref().map(|family| family.0.as_str()),
        Some(expected_family.to_string_lossy().as_ref())
    );
}

#[test]
fn clone_child_def_repo_does_not_overwrite_root_worktree() {
    // Real git trace2 output shows child processes (remote-https, index-pack)
    // emit def_repo with the CWD as worktree, not the clone destination.
    // The root process's def_repo has the correct newly-created repo path.
    // Verify that child def_repo events don't clobber the root's worktree.
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let cwd = temp.path().join("projects"); // non-repo CWD
    let clone_dest = cwd.join("testing-git"); // the clone destination
    crate::operations::git::test_utils::seed_valid_git_dir(&clone_dest.join(".git"));

    let root_sid = "20260327T000000.000000Z-Hdeadbeef-P00010000";
    let child_sid = format!("{}/20260327T000000.000001Z-Hdeadbeef-P00010001", root_sid);

    let start = serde_json::json!({
        "event": "start",
        "sid": root_sid,
        "ts": 1,
        "argv": ["git", "clone", "https://github.com/svarlamov/testing-git"]
        // No worktree or cwd — matches real trace2 start from non-repo dir
    });
    // Root def_repo: correct clone destination
    let root_def_repo = serde_json::json!({
        "event": "def_repo",
        "sid": root_sid,
        "ts": 2,
        "worktree": clone_dest
    });
    // Child def_repo from remote-https: reports CWD (parent), not destination
    let child_def_repo = serde_json::json!({
        "event": "def_repo",
        "sid": child_sid,
        "ts": 3,
        "worktree": cwd
    });
    let exit = serde_json::json!({
        "event": "exit",
        "sid": root_sid,
        "ts": 4,
        "code": 0
    });
    let atexit = atexit_payload(root_sid, 5);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&root_def_repo).unwrap().is_none());
    // Child def_repo must NOT overwrite the root worktree
    assert!(
        normalizer
            .ingest_payload(&child_def_repo)
            .unwrap()
            .is_none()
    );

    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
    assert_eq!(cmd.primary_command.as_deref(), Some("clone"));
    assert_eq!(
        cmd.worktree.as_ref(),
        Some(&clone_dest),
        "clone worktree should be the destination, not the parent CWD"
    );
}

#[test]
fn no_repo_routes_to_global_scope() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);

    let start = serde_json::json!({
        "event":"start",
        "sid":"s4",
        "ts":1,
        "argv":["git","version"]
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"s4",
        "ts":2,
        "code":0
    });
    let atexit = atexit_payload("s4", 3);

    normalizer.ingest_payload(&start).unwrap();
    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
    assert!(matches!(cmd.scope, CommandScope::Global));
}

#[test]
fn ignores_non_supported_trace_events() {
    use super::tests_lifecycle::payload;
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let p = payload("region_enter", "s5", 1);
    assert!(normalizer.ingest_payload(&p).unwrap().is_none());
}

#[test]
fn interleaved_roots_with_out_of_order_exits_finalize_independently() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_a = temp.path().join("repo-a");
    let repo_b = temp.path().join("repo-b");
    crate::operations::git::test_utils::seed_valid_git_dir(&repo_a.join(".git"));
    crate::operations::git::test_utils::seed_valid_git_dir(&repo_b.join(".git"));

    let start_a = serde_json::json!({
        "event":"start",
        "sid":"s-a",
        "ts":1,
        "argv":["git","commit","-m","a"],
        "worktree":repo_a
    });
    let start_b = serde_json::json!({
        "event":"start",
        "sid":"s-b",
        "ts":2,
        "argv":["git","push","origin","main"],
        "worktree":repo_b
    });
    let exit_b = serde_json::json!({
        "event":"exit",
        "sid":"s-b",
        "ts":3,
        "code":0
    });
    let exit_a = serde_json::json!({
        "event":"exit",
        "sid":"s-a",
        "ts":4,
        "code":0
    });
    let atexit_b = atexit_payload("s-b", 5);
    let atexit_a = atexit_payload("s-a", 6);

    assert!(normalizer.ingest_payload(&start_a).unwrap().is_none());
    assert!(normalizer.ingest_payload(&start_b).unwrap().is_none());

    assert!(normalizer.ingest_payload(&exit_b).unwrap().is_none());
    let cmd_b = normalizer.ingest_payload(&atexit_b).unwrap().unwrap();
    assert_eq!(cmd_b.root_sid, "s-b");
    assert_eq!(cmd_b.primary_command.as_deref(), Some("push"));
    assert_eq!(cmd_b.worktree.as_deref(), Some(repo_b.as_path()));
    assert!(matches!(cmd_b.scope, CommandScope::Family(_)));

    assert!(normalizer.ingest_payload(&exit_a).unwrap().is_none());
    let cmd_a = normalizer.ingest_payload(&atexit_a).unwrap().unwrap();
    assert_eq!(cmd_a.root_sid, "s-a");
    assert_eq!(cmd_a.primary_command.as_deref(), Some("commit"));
    assert_eq!(cmd_a.worktree.as_deref(), Some(repo_a.as_path()));
    assert!(matches!(cmd_a.scope, CommandScope::Family(_)));

    assert!(normalizer.state().pending.is_empty());
}

#[test]
fn start_ignores_repo_gitdir_hint_and_uses_cwd_for_worktree_resolution() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend.clone());
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo_base = temp.path().join("repo-base");
    let common_git_dir = repo_base.join(".git");
    let worker_git_dir = common_git_dir.join("worktrees").join("worker-b");
    let worker_worktree = temp.path().join("repo-worker-b");
    let worker_head = "1111111111111111111111111111111111111111";
    crate::operations::git::test_utils::seed_valid_git_dir(&common_git_dir);
    fs::create_dir_all(common_git_dir.join("refs").join("heads"))
        .expect("create common refs/heads");
    fs::create_dir_all(&worker_git_dir).expect("create linked worktree git dir");
    fs::create_dir_all(&worker_worktree).expect("create worker worktree");
    fs::write(
        worker_worktree.join(".git"),
        format!("gitdir: {}\n", worker_git_dir.display()),
    )
    .expect("write linked worktree .git pointer");
    fs::write(worker_git_dir.join("HEAD"), "ref: refs/heads/worker-b\n")
        .expect("write worker HEAD");
    fs::write(
        common_git_dir.join("refs").join("heads").join("worker-b"),
        format!("{worker_head}\n"),
    )
    .expect("write worker branch ref");

    let start = serde_json::json!({
        "event":"start",
        "sid":"s-repo-field",
        "ts":1,
        "argv":["git","commit","-m","msg"],
        "repo":common_git_dir,
        "cwd":worker_worktree
    });
    let def_repo = serde_json::json!({
        "event":"def_repo",
        "sid":"s-repo-field",
        "ts":2,
        "repo":worker_git_dir
    });
    let cmd_name = serde_json::json!({
        "event":"cmd_name",
        "sid":"s-repo-field",
        "ts":3,
        "name":"commit"
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"s-repo-field",
        "ts":4,
        "code":0
    });
    let atexit = atexit_payload("s-repo-field", 5);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&def_repo).unwrap().is_none());
    assert!(normalizer.ingest_payload(&cmd_name).unwrap().is_none());

    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer.ingest_payload(&atexit).unwrap().unwrap();
    assert_eq!(cmd.worktree.as_deref(), Some(worker_worktree.as_path()));
}

#[test]
fn destructive_stash_can_normalize_without_pre_command_target_oid() {
    let backend = Arc::new(MockBackend::default());
    let mut normalizer = TraceNormalizer::new(backend);
    let temp = tempfile::tempdir().expect("create tempdir");
    let repo = temp.path().join("repo");
    crate::operations::git::test_utils::seed_valid_git_dir(&repo.join(".git"));

    let start = serde_json::json!({
        "event":"start",
        "sid":"stash-missing-meta",
        "ts":1,
        "argv":["git","stash","pop"],
        "worktree":repo
    });
    let exit = serde_json::json!({
        "event":"exit",
        "sid":"stash-missing-meta",
        "ts":2,
        "code":0
    });
    let atexit = atexit_payload("stash-missing-meta", 3);

    assert!(normalizer.ingest_payload(&start).unwrap().is_none());
    assert!(normalizer.ingest_payload(&exit).unwrap().is_none());
    let cmd = normalizer
        .ingest_payload(&atexit)
        .expect("missing stash metadata should not block normalization")
        .expect("atexit payload should emit a normalized command");
    assert!(cmd.stash_target_oid.is_none());
}
