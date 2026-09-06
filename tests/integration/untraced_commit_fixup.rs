//! Best-effort attribution for commits the daemon never saw through trace2:
//! made while it was off, by clients that emit no trace2 (JGit, libgit2), or
//! from sandboxes that cannot reach its socket. The fixup pass claims such
//! commits from the worktree HEAD reflog and runs the normal post-commit path.

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use crate::test_utils::{
    codex_checkpoint, committed_metric_for_commit, committed_metrics_for_commit,
    isolated_metrics_db_path,
};
use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::metrics::events::committed_pos;
use serde_json::Value;
use std::fs;
use std::path::Path;

const TRACE2_DISABLED_ENV: [(&str, &str); 3] = [
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

/// Daemon environment for these tests: the fixup pass claims records
/// immediately (no minimum age) and metrics land in an isolated db.
fn fixup_daemon_env(metrics_db_path: &str) -> Vec<(&str, &str)> {
    vec![
        ("GIT_AI_TEST_METRICS_DB_PATH", metrics_db_path),
        ("GIT_AI_DAEMON_UNTRACED_FIXUP_MIN_AGE_MS", "0"),
    ]
}

/// A repository with a dedicated daemon running under `fixup_daemon_env`.
fn fixup_repo() -> (tempfile::TempDir, String, TestRepo) {
    let (metrics_dir, metrics_db_path) = isolated_metrics_db_path();
    let repo = TestRepo::new_with_daemon_env(&fixup_daemon_env(&metrics_db_path));
    (metrics_dir, metrics_db_path, repo)
}

/// Git that the daemon never hears about: no trace2 at all.
fn raw_git(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &TRACE2_DISABLED_ENV)
        .unwrap_or_else(|error| panic!("raw trace-disabled git {:?} failed: {}", args, error))
}

fn raw_head(repo: &TestRepo) -> String {
    raw_git(repo, &["rev-parse", "HEAD"]).trim().to_string()
}

fn raw_commit_all(repo: &TestRepo, message: &str) -> String {
    raw_git(repo, &["add", "-A"]);
    raw_git(repo, &["commit", "-m", message]);
    raw_head(repo)
}

fn write_file(repo: &TestRepo, path: &str, content: &str) {
    fs::write(repo.path().join(path), content).unwrap();
}

/// Records one codex edit of `path` through the daemon and waits for it to be
/// processed: the working log now carries AI attribution for the lines that
/// changed.
fn codex_edit(repo: &TestRepo, path: &str, content: &str, tool_use_id: &str) {
    let file_path = repo.path().join(path);
    codex_checkpoint(repo, &file_path, "fixup-session", "PreToolUse", tool_use_id);
    fs::write(&file_path, content).unwrap();
    codex_checkpoint(
        repo,
        &file_path,
        "fixup-session",
        "PostToolUse",
        tool_use_id,
    );
    repo.sync_daemon();
}

fn commit_source(db_path: &str, commit_sha: &str) -> Option<&'static str> {
    let event = committed_metric_for_commit(db_path, commit_sha);
    match event.values.get(&committed_pos::COMMIT_SOURCE.to_string()) {
        Some(Value::String(source)) if source == "untraced_fixup" => Some("untraced_fixup"),
        Some(Value::Null) | None => None,
        other => panic!("unexpected commit_source {other:?}"),
    }
}

fn health_counter(repo: &TestRepo, field: &str) -> u64 {
    repo.daemon_status()
        .get(field)
        .and_then(Value::as_u64)
        .unwrap_or_else(|| panic!("status.daemon lacks {field}"))
}

fn working_log_dir(repo: &TestRepo, base_commit: &str) -> std::path::PathBuf {
    repo.path()
        .join(".git")
        .join("ai")
        .join("working_logs")
        .join(base_commit)
}

#[test]
fn untraced_commit_with_delivered_checkpoints_is_attributed_after_scan() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "agent.txt", "base\n");
    let base = repo
        .stage_all_and_commit("traced base")
        .expect("traced base commit")
        .commit_sha;
    repo.request_untraced_fixup_scan();

    // The agent's hooks reached the daemon, but its commit (JGit, sandbox,
    // daemon down) never did.
    codex_edit(&repo, "agent.txt", "base\nai line\n", "tool-use-1");
    assert!(working_log_dir(&repo, &base).is_dir());
    let untraced = raw_commit_all(&repo, "agent commit");
    assert!(repo.read_authorship_note(&untraced).is_none());

    repo.request_untraced_fixup_scan();

    let mut file = repo.filename("agent.txt");
    file.assert_committed_lines(lines!["base".unattributed_human(), "ai line".ai()]);
    assert!(
        !working_log_dir(&repo, &base).is_dir(),
        "the working log is consumed exactly like the traced path does"
    );
    assert_eq!(
        commit_source(&metrics_db_path, &untraced),
        Some("untraced_fixup")
    );
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 1);

    // A second pass is a no-op: one note, one metric row.
    repo.request_untraced_fixup_scan();
    assert_eq!(
        committed_metrics_for_commit(&metrics_db_path, &untraced).len(),
        1
    );
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 1);
}

#[test]
fn traced_commits_keep_a_null_commit_source_and_are_never_reclaimed() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "agent.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    codex_edit(&repo, "agent.txt", "base\nai line\n", "tool-use-1");
    let traced = repo
        .stage_all_and_commit("traced agent commit")
        .expect("traced agent commit")
        .commit_sha;
    assert_eq!(commit_source(&metrics_db_path, &traced), None);

    repo.request_untraced_fixup_scan();
    repo.request_untraced_fixup_scan();

    let mut file = repo.filename("agent.txt");
    file.assert_committed_lines(lines!["base".unattributed_human(), "ai line".ai()]);
    assert_eq!(
        committed_metrics_for_commit(&metrics_db_path, &traced).len(),
        1
    );
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
}

#[test]
fn untraced_commit_without_checkpoints_gets_a_note_from_recovery() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "plain.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    repo.request_untraced_fixup_scan();

    write_file(&repo, "plain.txt", "base\nhand written\n");
    let untraced = raw_commit_all(&repo, "typed by hand");

    repo.request_untraced_fixup_scan();

    assert!(
        repo.read_authorship_note(&untraced).is_some(),
        "the normal post-commit path writes a note even without a working log"
    );
    let mut file = repo.filename("plain.txt");
    file.assert_committed_lines(lines![
        "base".unattributed_human(),
        "hand written".unattributed_human()
    ]);
    assert_eq!(
        commit_source(&metrics_db_path, &untraced),
        Some("untraced_fixup")
    );
}

#[test]
fn first_sighting_of_a_repository_never_backfills_history() {
    let (_metrics_dir, _metrics_db_path, repo) = fixup_repo();
    // The daemon is running but has never heard of this repository: these
    // commits are history by the time it first looks.
    write_file(&repo, "old.txt", "written before git-ai knew this repo\n");
    raw_commit_all(&repo, "pre-existing untraced history");
    write_file(
        &repo,
        "old.txt",
        "written before git-ai knew this repo\nstill before\n",
    );
    let before_first_sighting = raw_commit_all(&repo, "more pre-existing history");

    repo.request_untraced_fixup_scan();
    repo.request_untraced_fixup_scan();

    assert!(
        repo.read_authorship_note(&before_first_sighting).is_none(),
        "commits from before the daemon knew the repo are history, not fixup work"
    );
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
    assert_eq!(health_counter(&repo, "untraced_commits_skipped"), 0);

    // Once known, the next untraced commit is claimed.
    write_file(&repo, "new.txt", "after first sighting\n");
    let after = raw_commit_all(&repo, "untraced after first sighting");
    repo.request_untraced_fixup_scan();
    assert!(repo.read_authorship_note(&after).is_some());
    let mut file = repo.filename("new.txt");
    file.assert_committed_lines(lines!["after first sighting".unattributed_human()]);
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 1);
}

#[test]
fn untraced_amend_is_a_rewrite_and_is_skipped() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "agent.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    codex_edit(&repo, "agent.txt", "base\nai line\n", "tool-use-1");
    let traced = repo
        .stage_all_and_commit("traced agent commit")
        .expect("traced agent commit")
        .commit_sha;
    repo.request_untraced_fixup_scan();

    raw_git(
        &repo,
        &["commit", "--amend", "-m", "amended while untraced"],
    );
    let amended = raw_head(&repo);
    assert_ne!(amended, traced);

    repo.request_untraced_fixup_scan();

    assert!(repo.read_authorship_note(&amended).is_none());
    assert!(repo.read_authorship_note(&traced).is_some());
    assert!(committed_metrics_for_commit(&metrics_db_path, &amended).is_empty());
    assert_eq!(health_counter(&repo, "untraced_commits_skipped"), 1);
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
}

#[test]
fn untraced_rebase_of_traced_commits_is_not_fixed_up() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "base.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    let main_branch = raw_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_string();
    repo.request_untraced_fixup_scan();

    repo.git(&["checkout", "-b", "topic"]).unwrap();
    write_file(&repo, "agent.txt", "");
    codex_edit(&repo, "agent.txt", "ai line\n", "tool-use-1");
    let topic_commit = repo
        .stage_all_and_commit("traced topic commit")
        .expect("traced topic commit")
        .commit_sha;
    let mut file = repo.filename("agent.txt");
    file.assert_committed_lines(lines!["ai line".ai()]);

    repo.git(&["checkout", &main_branch]).unwrap();
    write_file(&repo, "main.txt", "main moved on\n");
    repo.stage_all_and_commit("traced main commit")
        .expect("traced main commit");

    // The rebase happens where the daemon cannot see it.
    raw_git(&repo, &["checkout", "topic"]);
    raw_git(&repo, &["rebase", &main_branch]);
    let rebased = raw_head(&repo);
    assert_ne!(rebased, topic_commit);

    repo.request_untraced_fixup_scan();

    assert!(
        repo.read_authorship_note(&rebased).is_none(),
        "rewrites are never fixed up; only genuinely new commits are"
    );
    assert!(repo.read_authorship_note(&topic_commit).is_some());
    assert!(committed_metrics_for_commit(&metrics_db_path, &rebased).is_empty());
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
}

#[test]
fn untraced_pull_of_remote_commits_is_not_fixed_up() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    let origin = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    write_file(&origin, "remote.txt", "remote base\n");
    raw_commit_all(&origin, "remote base");
    let branch = raw_git(&origin, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_string();
    raw_git(
        &repo,
        &[
            "remote",
            "add",
            "origin",
            origin.path().to_str().expect("origin path is utf-8"),
        ],
    );
    repo.request_untraced_fixup_scan();

    write_file(
        &origin,
        "remote.txt",
        "remote base\nmade on another machine\n",
    );
    let remote_commit = raw_commit_all(&origin, "remote commit");
    raw_git(&repo, &["pull", "--ff-only", "origin", &branch]);
    assert_eq!(raw_head(&repo), remote_commit);

    repo.request_untraced_fixup_scan();

    assert!(
        repo.read_authorship_note(&remote_commit).is_none(),
        "pulled commits were not made on this machine"
    );
    assert!(committed_metrics_for_commit(&metrics_db_path, &remote_commit).is_empty());
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
}

#[test]
fn untraced_detached_head_commit_is_skipped() {
    let (_metrics_dir, _metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "base.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    repo.request_untraced_fixup_scan();

    raw_git(&repo, &["checkout", "--detach"]);
    write_file(&repo, "detached.txt", "on no branch\n");
    let detached = raw_commit_all(&repo, "detached commit");

    repo.request_untraced_fixup_scan();

    assert!(repo.read_authorship_note(&detached).is_none());
    // Both the `checkout:` record and the detached commit are settled unclaimed.
    assert_eq!(health_counter(&repo, "untraced_commits_skipped"), 2);
}

#[test]
fn fixup_scan_for_one_repository_covers_every_worktree_of_its_family() {
    let (_metrics_dir, _metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "base.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    let scheduled = repo.request_untraced_fixup_scan();
    assert_eq!(scheduled.get("worktrees").and_then(Value::as_u64), Some(1));
    assert!(Path::new(&repo.path().join(".git")).is_dir());

    let linked = repo.path().parent().expect("temp parent").join(format!(
        "{}-linked",
        repo.path().file_name().unwrap().to_string_lossy()
    ));
    raw_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf-8 path"),
        ],
    );
    let scheduled = repo.request_untraced_fixup_scan();
    assert_eq!(
        scheduled.get("worktrees").and_then(Value::as_u64),
        Some(2),
        "a request naming one worktree scans the whole family"
    );
    let _ = fs::remove_dir_all(&linked);
}

#[test]
fn commit_made_while_the_daemon_was_off_is_attributed_after_restart() {
    let (_metrics_dir, metrics_db_path, mut repo) = fixup_repo();
    write_file(&repo, "agent.txt", "base\n");
    let base = repo
        .stage_all_and_commit("traced base")
        .expect("traced base commit")
        .commit_sha;
    // The daemon has seen this repository: it is a known family.
    repo.request_untraced_fixup_scan();
    codex_edit(&repo, "agent.txt", "base\nai line\n", "tool-use-1");
    repo.sync_daemon();

    repo.shutdown_dedicated_daemon_for_test();
    let while_off = raw_commit_all(&repo, "committed while the daemon was down");
    assert!(working_log_dir(&repo, &base).is_dir());
    repo.start_dedicated_daemon_with_env_for_test(&fixup_daemon_env(&metrics_db_path));

    // A fresh daemon knows this repository only through the persisted store.
    let scheduled = repo.request_untraced_fixup_scan_all();
    assert_eq!(scheduled.get("families").and_then(Value::as_u64), Some(1));

    let mut file = repo.filename("agent.txt");
    file.assert_committed_lines(lines!["base".unattributed_human(), "ai line".ai()]);
    assert!(!working_log_dir(&repo, &base).is_dir());
    assert_eq!(
        commit_source(&metrics_db_path, &while_off),
        Some("untraced_fixup")
    );
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 1);
}

#[test]
fn rebase_made_while_the_daemon_was_off_is_not_fixed_up() {
    let (_metrics_dir, metrics_db_path, mut repo) = fixup_repo();
    write_file(&repo, "base.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    let main_branch = raw_git(&repo, &["rev-parse", "--abbrev-ref", "HEAD"])
        .trim()
        .to_string();
    repo.git(&["checkout", "-b", "topic"]).unwrap();
    write_file(&repo, "agent.txt", "");
    codex_edit(&repo, "agent.txt", "ai line\n", "tool-use-1");
    let topic_commit = repo
        .stage_all_and_commit("traced topic commit")
        .expect("traced topic commit")
        .commit_sha;
    repo.git(&["checkout", &main_branch]).unwrap();
    write_file(&repo, "main.txt", "main moved on\n");
    repo.stage_all_and_commit("traced main commit")
        .expect("traced main commit");
    repo.request_untraced_fixup_scan();

    repo.shutdown_dedicated_daemon_for_test();
    raw_git(&repo, &["checkout", "topic"]);
    raw_git(&repo, &["rebase", &main_branch]);
    let rebased = raw_head(&repo);
    assert_ne!(rebased, topic_commit);
    repo.start_dedicated_daemon_with_env_for_test(&fixup_daemon_env(&metrics_db_path));

    repo.request_untraced_fixup_scan_all();

    assert!(repo.read_authorship_note(&rebased).is_none());
    assert!(repo.read_authorship_note(&topic_commit).is_some());
    assert!(committed_metrics_for_commit(&metrics_db_path, &rebased).is_empty());
    assert_eq!(health_counter(&repo, "untraced_commits_fixed"), 0);
}

#[test]
fn untraced_commit_in_a_linked_worktree_is_attributed() {
    let (_metrics_dir, metrics_db_path, repo) = fixup_repo();
    write_file(&repo, "base.txt", "base\n");
    repo.stage_all_and_commit("traced base")
        .expect("traced base commit");
    let linked = repo.path().parent().expect("temp parent").join(format!(
        "{}-linked",
        repo.path().file_name().unwrap().to_string_lossy()
    ));
    raw_git(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("utf-8 path"),
        ],
    );
    // Scanning every worktree of the family seeds the linked one too.
    let scheduled = repo.request_untraced_fixup_scan_all();
    assert_eq!(scheduled.get("worktrees").and_then(Value::as_u64), Some(2));

    fs::write(linked.join("linked.txt"), "from the linked worktree\n").unwrap();
    let git_in_linked = |args: &[&str]| {
        let mut command = std::process::Command::new("git");
        command.arg("-C").arg(&linked).args(args);
        for (key, value) in TRACE2_DISABLED_ENV {
            command.env(key, value);
        }
        let output = command.output().expect("git in linked worktree");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    };
    git_in_linked(&["add", "-A"]);
    git_in_linked(&["commit", "-m", "untraced in linked worktree"]);
    let in_linked = git_in_linked(&["rev-parse", "HEAD"]);

    repo.request_untraced_fixup_scan_all();

    let note = repo
        .read_authorship_note(&in_linked)
        .expect("the linked worktree commit has a note");
    let log = AuthorshipLog::deserialize_from_string(&note).expect("note parses");
    assert_eq!(log.metadata.base_commit_sha, in_linked);
    assert!(
        log.attestations
            .iter()
            .flat_map(|attestation| &attestation.entries)
            .all(|entry| entry.hash == "human" || entry.hash.starts_with("h_")),
        "a hand-typed line never gets AI attribution: {:?}",
        log.attestations
    );
    assert_eq!(
        commit_source(&metrics_db_path, &in_linked),
        Some("untraced_fixup")
    );
    let _ = fs::remove_dir_all(&linked);
}
