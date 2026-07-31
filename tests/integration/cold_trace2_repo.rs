use git_ai::authorship::authorship_log_serialization::AuthorshipLog;

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use std::fs;

const TRACE2_DISABLED_ENV: [(&str, &str); 3] = [
    ("GIT_TRACE2", "0"),
    ("GIT_TRACE2_EVENT", "0"),
    ("GIT_TRACE2_PERF", "0"),
];

fn cold_repo() -> TestRepo {
    TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon)
}

fn raw_git(repo: &TestRepo, args: &[&str]) -> String {
    repo.git_og_with_env(args, &TRACE2_DISABLED_ENV)
        .unwrap_or_else(|error| panic!("raw trace-disabled git {:?} failed: {}", args, error))
}

fn raw_git_result(repo: &TestRepo, args: &[&str]) -> Result<String, String> {
    repo.git_og_with_env(args, &TRACE2_DISABLED_ENV)
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
    let full_path = repo.path().join(path);
    if let Some(parent) = full_path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(full_path, content).unwrap();
}

fn raw_commit_file(repo: &TestRepo, path: &str, content: &str, message: &str) -> String {
    write_file(repo, path, content);
    raw_commit_all(repo, message)
}

fn raw_clone(source: &TestRepo, target_path: &std::path::Path) -> TestRepo {
    raw_git(
        source,
        &[
            "clone",
            source.path().to_str().expect("source path should be utf-8"),
            target_path.to_str().expect("target path should be utf-8"),
        ],
    );
    TestRepo::new_at_path_with_daemon_scope(target_path, DaemonTestScope::NoDaemon)
}

fn traced_ai_commit_file(repo: &TestRepo, path: &str, content: &str, message: &str) -> String {
    write_file(repo, path, content);
    repo.git_ai(&["checkpoint", "mock_ai", path])
        .unwrap_or_else(|error| panic!("mock_ai checkpoint for {} failed: {}", path, error));
    repo.stage_all_and_commit(message)
        .unwrap_or_else(|error| panic!("commit {} failed: {}", message, error))
        .commit_sha
}

fn read_file(repo: &TestRepo, path: &str) -> String {
    fs::read_to_string(repo.path().join(path)).unwrap()
}

fn start_cold_daemon(repo: &mut TestRepo) {
    repo.start_dedicated_daemon_for_test();
}

fn run_traced_git(repo: &TestRepo, args: &[&str]) -> String {
    let output = run_traced_git_without_sync(repo, args);
    repo.sync_daemon_force();
    output
}

fn run_traced_git_without_sync(repo: &TestRepo, args: &[&str]) -> String {
    assert!(
        repo.git_command_affects_daemon_for_tracking(args, None),
        "git {:?} should be tracked by daemon test sync",
        args
    );
    repo.git(args)
        .unwrap_or_else(|error| panic!("traced git {:?} failed: {}", args, error))
}

fn assert_ai_authorship_note(repo: &TestRepo, commit_sha: &str) {
    let note = repo
        .read_authorship_note(commit_sha)
        .unwrap_or_else(|| panic!("commit {commit_sha} should have an authorship note"));
    let log = AuthorshipLog::deserialize_from_string(&note)
        .unwrap_or_else(|error| panic!("failed to parse authorship note: {}", error));
    assert!(
        log.attestations
            .iter()
            .any(|attestation| !attestation.entries.is_empty()),
        "commit {commit_sha} should contain AI authorship entries"
    );
}

fn assert_no_ai_authorship_for_commit(repo: &TestRepo, commit_sha: &str) {
    let Some(note) = repo.read_authorship_note(commit_sha) else {
        return;
    };
    assert_note_has_no_ai_authorship(commit_sha, &note);
}

fn assert_no_authorship_note(repo: &TestRepo, commit_sha: &str) {
    assert!(
        repo.read_authorship_note(commit_sha).is_none(),
        "commit {commit_sha} should not have an authorship note"
    );
}

fn assert_traced_commit_has_no_ai_authorship(repo: &TestRepo, commit_sha: &str) {
    let note = repo
        .read_authorship_note(commit_sha)
        .unwrap_or_else(|| panic!("traced commit {commit_sha} should have an authorship note"));
    assert_note_has_no_ai_authorship(commit_sha, &note);
}

fn assert_note_has_no_ai_authorship(commit_sha: &str, note: &str) {
    let log = AuthorshipLog::deserialize_from_string(note)
        .unwrap_or_else(|error| panic!("failed to parse authorship note: {}", error));
    assert!(
        log.attestations
            .iter()
            .flat_map(|attestation| &attestation.entries)
            .all(|entry| entry.hash == "human" || entry.hash.starts_with("h_")),
        "cold raw setup should not create AI attestations for {}: {:?}",
        commit_sha,
        log.attestations
    );
    assert!(
        log.metadata.prompts.is_empty() && log.metadata.sessions.is_empty(),
        "cold raw setup should not create AI metadata for {}: {:?}",
        commit_sha,
        log.metadata
    );
}

#[test]
fn test_cold_repo_first_traced_commit_is_processed() {
    let mut repo = cold_repo();
    let raw_first = raw_commit_file(&repo, "history.txt", "base\n", "raw base");
    let raw_second = raw_commit_file(&repo, "history.txt", "base\nraw\n", "raw second");
    write_file(&repo, "traced.txt", "first traced commit\n");
    raw_git(&repo, &["add", "traced.txt"]);

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["commit", "-m", "first traced commit"]);

    let head = raw_head(&repo);
    assert_ne!(head, raw_second);
    assert_eq!(read_file(&repo, "traced.txt"), "first traced commit\n");
    assert_no_ai_authorship_for_commit(&repo, &raw_first);
    assert_no_ai_authorship_for_commit(&repo, &raw_second);
    assert_no_ai_authorship_for_commit(&repo, &head);
}

#[test]
fn test_cold_repo_commit_message_trailing_whitespace_preserves_ai_authorship() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "tracked.txt", "base\n", "raw base");

    start_cold_daemon(&mut repo);
    write_file(&repo, "tracked.txt", "base\nAI line\n");
    repo.git_ai(&["checkpoint", "mock_ai", "tracked.txt"])
        .expect("mock_ai checkpoint should succeed");
    repo.git(&["add", "tracked.txt"])
        .expect("staging AI change should succeed");
    run_traced_git(&repo, &["commit", "-m", "AI change "]);

    assert_ai_authorship_note(&repo, &raw_head(&repo));
    let stats = repo.stats().expect("commit stats should be available");
    assert_eq!(stats.ai_additions, 1);
    assert_eq!(stats.unknown_additions, 0);
}

fn run_cold_repo_first_traced_pull_rebase_preserves_rebased_ai_authorship() {
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    raw_git(&upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let mut repo = cold_repo();
    raw_git(&repo, &["branch", "-M", "main"]);
    raw_git(
        &repo,
        &["remote", "add", "origin", upstream.path().to_str().unwrap()],
    );
    raw_commit_file(&repo, "README.md", "# Test Repo\n", "raw initial");
    raw_git(&repo, &["push", "-u", "origin", "HEAD:main"]);

    start_cold_daemon(&mut repo);
    let local_ai_commit = traced_ai_commit_file(
        &repo,
        "ai_feature.txt",
        "AI generated feature line 1\nAI generated feature line 2\n",
        "add AI feature",
    );
    assert_ai_authorship_note(&repo, &local_ai_commit);

    let contributor_parent = tempfile::tempdir().expect("contributor temp dir");
    let contributor_path = contributor_parent.path().join("contributor");
    let contributor = raw_clone(&upstream, &contributor_path);
    raw_git(&contributor, &["checkout", "main"]);
    raw_commit_file(
        &contributor,
        "upstream_change.txt",
        "upstream content\n",
        "upstream divergent commit",
    );
    raw_git(&contributor, &["push", "origin", "HEAD:main"]);

    assert!(
        repo.git(&["push"]).is_err(),
        "push should be rejected because origin has diverged"
    );
    assert!(
        repo.git(&["pull"]).is_err(),
        "plain pull should fail before an explicit reconciliation strategy"
    );
    repo.git(&["pull", "--rebase"])
        .expect("pull --rebase should succeed");
    repo.sync_daemon_force();

    let rebased = raw_head(&repo);
    assert_ne!(rebased, local_ai_commit);
    assert_ai_authorship_note(&repo, &rebased);
}

#[test]
fn test_cold_repo_first_traced_pull_rebase_preserves_rebased_ai_authorship() {
    run_cold_repo_first_traced_pull_rebase_preserves_rebased_ai_authorship();
}

#[test]
#[ignore = "stress test for nondeterministic cold pull-rebase reflog timing"]
fn stress_cold_repo_first_traced_pull_rebase_preserves_rebased_ai_authorship() {
    std::thread::scope(|scope| {
        let handles = (0..8)
            .map(|_| {
                scope.spawn(|| {
                    for _ in 0..3 {
                        run_cold_repo_first_traced_pull_rebase_preserves_rebased_ai_authorship();
                    }
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle
                .join()
                .expect("cold pull-rebase stress worker panicked");
        }
    });
}

#[test]
fn test_traced_commit_after_untraced_head_move_creates_authorship_note() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git(&["add", "base.txt"]).unwrap();
    run_traced_git(&repo, &["commit", "-m", "traced base"]);
    let traced_base = raw_head(&repo);
    assert_traced_commit_has_no_ai_authorship(&repo, &traced_base);

    let raw_unseen = raw_commit_file(&repo, "raw.txt", "raw unseen\n", "raw unseen");
    assert_no_ai_authorship_for_commit(&repo, &raw_unseen);

    write_file(&repo, "next.txt", "next traced\n");
    repo.git(&["add", "next.txt"]).unwrap();
    run_traced_git(&repo, &["commit", "-m", "traced after raw"]);
    let traced_after_raw = raw_head(&repo);

    assert_traced_commit_has_no_ai_authorship(&repo, &traced_after_raw);
}

#[test]
fn test_traced_commit_after_untraced_duplicate_message_head_move_notes_traced_commit() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git(&["add", "base.txt"]).unwrap();
    run_traced_git(&repo, &["commit", "-m", "traced base"]);
    let traced_base = raw_head(&repo);
    assert_traced_commit_has_no_ai_authorship(&repo, &traced_base);

    let raw_unseen = raw_commit_file(&repo, "raw.txt", "raw unseen\n", "same message");
    assert_no_authorship_note(&repo, &raw_unseen);

    write_file(&repo, "next.txt", "next traced\n");
    repo.git(&["add", "next.txt"]).unwrap();
    run_traced_git(&repo, &["commit", "-m", "same message"]);
    let traced_after_raw = raw_head(&repo);

    assert_no_authorship_note(&repo, &raw_unseen);
    assert_traced_commit_has_no_ai_authorship(&repo, &traced_after_raw);
}

#[test]
fn test_cold_repo_first_traced_amend_is_processed() {
    let mut repo = cold_repo();
    let original = raw_commit_file(&repo, "amend.txt", "before\n", "raw before amend");
    write_file(&repo, "amend.txt", "before\namended\n");
    raw_git(&repo, &["add", "amend.txt"]);

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["commit", "--amend", "--no-edit"]);

    let amended = raw_head(&repo);
    assert_ne!(amended, original);
    assert_eq!(read_file(&repo, "amend.txt"), "before\namended\n");
    assert_no_ai_authorship_for_commit(&repo, &amended);
}

#[test]
fn test_cold_repo_first_traced_soft_reset_is_processed() {
    let mut repo = cold_repo();
    let first = raw_commit_file(&repo, "reset.txt", "one\n", "raw reset base");
    let second = raw_commit_file(&repo, "reset.txt", "one\ntwo\n", "raw reset advance");

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["reset", "--soft", &first]);

    assert_eq!(raw_head(&repo), first);
    assert_eq!(read_file(&repo, "reset.txt"), "one\ntwo\n");
    let staged = raw_git(&repo, &["diff", "--cached", "--name-only"]);
    assert!(
        staged.lines().any(|line| line == "reset.txt"),
        "soft reset should leave reset.txt staged, got: {}",
        staged
    );
    assert_no_ai_authorship_for_commit(&repo, &second);
}

#[test]
fn test_cold_repo_first_traced_rebase_is_processed() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "base.txt", "base\n", "raw base");
    raw_git(&repo, &["branch", "-M", "main"]);
    raw_git(&repo, &["checkout", "-b", "feature"]);
    let old_feature = raw_commit_file(&repo, "feature.txt", "feature\n", "raw feature");
    raw_git(&repo, &["checkout", "main"]);
    let main_tip = raw_commit_file(&repo, "main.txt", "main\n", "raw main advance");
    raw_git(&repo, &["checkout", "feature"]);

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["rebase", "main"]);

    let rebased = raw_head(&repo);
    assert_ne!(rebased, old_feature);
    raw_git(&repo, &["merge-base", "--is-ancestor", &main_tip, "HEAD"]);
    assert_eq!(read_file(&repo, "feature.txt"), "feature\n");
    assert_no_ai_authorship_for_commit(&repo, &rebased);
}

#[test]
fn test_cold_repo_first_traced_conflict_rebase_ignores_stale_rebase_reflog_history() {
    let mut repo = TestRepo::new_dedicated_daemon();
    traced_ai_commit_file(&repo, "base.txt", "base\n", "ai base");
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "old-topic"]).unwrap();
    traced_ai_commit_file(&repo, "old.txt", "old topic\n", "ai old topic");
    repo.git(&["checkout", "main"]).unwrap();
    traced_ai_commit_file(&repo, "main.txt", "main advance\n", "ai main advance");
    repo.git(&["checkout", "old-topic"]).unwrap();
    repo.git(&["rebase", "main"]).unwrap();
    repo.git(&["checkout", "main"]).unwrap();

    traced_ai_commit_file(
        &repo,
        "jokes-animals.csv",
        "setup,punchline\nWhat do you call a bear with no teeth?,A gummy bear\n",
        "ai initial jokes",
    );
    repo.git(&["checkout", "-b", "scenario-3-multi-file-conflict"])
        .unwrap();
    let feature_tip = traced_ai_commit_file(
        &repo,
        "jokes-animals.csv",
        "setup,punchline\nWhat do you call a bear with no teeth?,A gummy bear\nWhat do you call a sleeping bull?,A dozer\n",
        "ai bull joke",
    );
    repo.git(&["checkout", "main"]).unwrap();
    traced_ai_commit_file(
        &repo,
        "jokes-animals.csv",
        "setup,punchline\nWhat do you call a bear with no teeth?,A gummy bear\nWhat's a cat's favorite color?,Purr-ple\n",
        "ai cat joke",
    );

    repo.restart_dedicated_daemon_for_test();
    let rebase = repo.git(&["rebase", "main", "scenario-3-multi-file-conflict"]);
    assert!(
        rebase.is_err(),
        "rebase should stop for a conflict, got: {:?}",
        rebase
    );
    write_file(
        &repo,
        "jokes-animals.csv",
        "setup,punchline\nWhat do you call a bear with no teeth?,A gummy bear\nWhat's a cat's favorite color?,Purr-ple\nWhat do you call a sleeping bull?,A dozer\n",
    );
    repo.git(&["add", "jokes-animals.csv"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();
    repo.sync_daemon_force();

    let rebased = raw_head(&repo);
    assert_ne!(rebased, feature_tip);
    let mut file = repo.filename("jokes-animals.csv");
    file.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "What do you call a bear with no teeth?,A gummy bear".ai(),
        "What's a cat's favorite color?,Purr-ple".ai(),
        "What do you call a sleeping bull?,A dozer".ai(),
    ]);
}

#[test]
fn test_cold_repo_mid_rebase_continue_preserves_ai_conflict_resolution() {
    let mut repo = TestRepo::new_dedicated_daemon();
    traced_ai_commit_file(&repo, "conflict.txt", "base\n", "ai base");
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    let feature_tip = traced_ai_commit_file(&repo, "conflict.txt", "feature\n", "ai feature");

    repo.git(&["checkout", "main"]).unwrap();
    traced_ai_commit_file(&repo, "conflict.txt", "main\n", "ai main");

    raw_git(&repo, &["checkout", "feature"]);
    let rebase = raw_git_result(&repo, &["rebase", "main"]);
    assert!(
        rebase.is_err(),
        "raw trace-disabled rebase should stop for conflict, got: {:?}",
        rebase
    );

    repo.restart_dedicated_daemon_for_test();
    repo.git_ai(&["checkpoint", "human", "conflict.txt"])
        .unwrap();
    write_file(&repo, "conflict.txt", "resolved by ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.txt"])
        .unwrap();
    repo.git(&["add", "conflict.txt"]).unwrap();
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .unwrap();
    repo.sync_daemon_force();

    let rebased = raw_head(&repo);
    assert_ne!(rebased, feature_tip);
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(crate::lines!["resolved by ai".ai()]);
}

#[test]
fn test_cold_repo_mid_cherry_pick_continue_preserves_ai_conflict_resolution() {
    let mut repo = TestRepo::new_dedicated_daemon();
    traced_ai_commit_file(&repo, "conflict.txt", "base\n", "ai base");
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "source"]).unwrap();
    let source_commit = traced_ai_commit_file(&repo, "conflict.txt", "source\n", "ai source");

    repo.git(&["checkout", "main"]).unwrap();
    traced_ai_commit_file(&repo, "conflict.txt", "main\n", "ai main");

    let cherry_pick = raw_git_result(&repo, &["cherry-pick", &source_commit]);
    assert!(
        cherry_pick.is_err(),
        "raw trace-disabled cherry-pick should stop for conflict, got: {:?}",
        cherry_pick
    );

    repo.restart_dedicated_daemon_for_test();
    repo.git_ai(&["checkpoint", "human", "conflict.txt"])
        .unwrap();
    write_file(&repo, "conflict.txt", "resolved by ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.txt"])
        .unwrap();
    repo.git(&["add", "conflict.txt"]).unwrap();
    repo.git_with_env(
        &["cherry-pick", "--continue"],
        &[("GIT_EDITOR", "true")],
        None,
    )
    .unwrap();
    repo.sync_daemon_force();

    let picked = raw_head(&repo);
    assert_ne!(picked, source_commit);
    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(crate::lines!["resolved by ai".ai()]);
}

#[test]
fn test_cold_repo_mid_merge_commit_preserves_ai_conflict_resolution() {
    let mut repo = TestRepo::new_dedicated_daemon();
    traced_ai_commit_file(&repo, "conflict.txt", "base\n", "ai base");
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    traced_ai_commit_file(&repo, "conflict.txt", "feature\n", "ai feature");

    repo.git(&["checkout", "main"]).unwrap();
    traced_ai_commit_file(&repo, "conflict.txt", "main\n", "ai main");

    let merge = raw_git_result(&repo, &["merge", "feature"]);
    assert!(
        merge.is_err(),
        "raw trace-disabled merge should stop for conflict, got: {:?}",
        merge
    );

    repo.restart_dedicated_daemon_for_test();
    repo.git_ai(&["checkpoint", "human", "conflict.txt"])
        .unwrap();
    write_file(&repo, "conflict.txt", "resolved by ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "conflict.txt"])
        .unwrap();
    repo.git(&["add", "conflict.txt"]).unwrap();
    repo.git(&["commit", "-m", "merge resolved"]).unwrap();
    repo.sync_daemon_force();

    let mut file = repo.filename("conflict.txt");
    file.assert_committed_lines(crate::lines!["resolved by ai".ai()]);
}

#[test]
fn test_cold_repo_first_traced_cherry_pick_is_processed() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "base.txt", "base\n", "raw base");
    raw_git(&repo, &["branch", "-M", "main"]);
    raw_git(&repo, &["checkout", "-b", "feature"]);
    let source = raw_commit_file(&repo, "picked.txt", "picked\n", "raw picked source");
    raw_git(&repo, &["checkout", "main"]);
    raw_commit_file(&repo, "main.txt", "main\n", "raw main advance");

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["cherry-pick", &source]);

    let picked = raw_head(&repo);
    assert_ne!(picked, source);
    assert_eq!(read_file(&repo, "picked.txt"), "picked\n");
    assert_no_ai_authorship_for_commit(&repo, &picked);
}

#[test]
fn test_cold_repo_first_traced_squash_merge_is_processed() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "base.txt", "base\n", "raw base");
    raw_git(&repo, &["branch", "-M", "main"]);
    raw_git(&repo, &["checkout", "-b", "feature"]);
    raw_commit_file(
        &repo,
        "feature.txt",
        "feature squash\n",
        "raw squash source",
    );
    raw_git(&repo, &["checkout", "main"]);
    raw_commit_file(&repo, "main.txt", "main\n", "raw main advance");

    start_cold_daemon(&mut repo);
    run_traced_git_without_sync(&repo, &["merge", "--squash", "feature"]);
    let staged = raw_git(&repo, &["diff", "--cached", "--name-only"]);
    assert!(
        staged.lines().any(|line| line == "feature.txt"),
        "squash merge should stage feature.txt, got: {}",
        staged
    );
    run_traced_git(&repo, &["commit", "-m", "first traced squash commit"]);

    let squash_commit = raw_head(&repo);
    assert_eq!(read_file(&repo, "feature.txt"), "feature squash\n");
    assert_no_ai_authorship_for_commit(&repo, &squash_commit);
}

#[test]
fn test_cold_daemon_first_traced_squash_merge_preserves_source_ai_authorship() {
    let mut repo = TestRepo::new_dedicated_daemon();
    let mut file = repo.filename("document.txt");

    file.set_contents(crate::lines![
        "section 1".unattributed_human(),
        "section 2".unattributed_human(),
        "section 3".unattributed_human()
    ]);
    repo.stage_all_and_commit("initial document").unwrap();
    repo.git(&["branch", "-M", "main"]).unwrap();

    repo.git(&["checkout", "-b", "feature"]).unwrap();
    file.insert_at(3, crate::lines!["// AI feature addition at end".ai()]);
    repo.stage_all_and_commit("AI adds feature").unwrap();

    repo.git(&["checkout", "main"]).unwrap();
    let mut file = repo.filename("document.txt");
    file.insert_at(
        0,
        crate::lines!["// Master update at top".unattributed_human()],
    );
    repo.stage_all_and_commit("out-of-band main update")
        .unwrap();

    repo.restart_dedicated_daemon_for_test();
    repo.git(&["merge", "--squash", "feature"]).unwrap();
    repo.stage_all_and_commit("squashed feature").unwrap();

    let mut file = repo.filename("document.txt");
    file.assert_committed_lines(crate::lines![
        "// Master update at top".human(),
        "section 1".human(),
        "section 2".human(),
        "section 3".ai(),
        "// AI feature addition at end".ai()
    ]);
}

#[test]
fn test_cold_repo_first_traced_merge_is_processed() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "base.txt", "base\n", "raw base");
    raw_git(&repo, &["branch", "-M", "main"]);
    raw_git(&repo, &["checkout", "-b", "feature"]);
    raw_commit_file(&repo, "feature.txt", "feature\n", "raw feature");
    raw_git(&repo, &["checkout", "main"]);
    raw_commit_file(&repo, "main.txt", "main\n", "raw main advance");

    start_cold_daemon(&mut repo);
    run_traced_git(
        &repo,
        &["merge", "--no-ff", "feature", "-m", "first traced merge"],
    );

    let merge_commit = raw_head(&repo);
    let parents = raw_git(&repo, &["rev-list", "--parents", "-n", "1", "HEAD"]);
    assert_eq!(
        parents.split_whitespace().count(),
        3,
        "merge commit should have two parents, got: {}",
        parents
    );
    assert_eq!(read_file(&repo, "feature.txt"), "feature\n");
    assert_no_ai_authorship_for_commit(&repo, &merge_commit);
}

#[test]
fn test_cold_repo_first_traced_stash_pop_is_processed() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "stash.txt", "base\n", "raw base");
    write_file(&repo, "stash.txt", "base\nstashed\n");
    raw_git(&repo, &["stash", "push", "-m", "raw stash"]);
    assert_eq!(read_file(&repo, "stash.txt"), "base\n");

    start_cold_daemon(&mut repo);
    run_traced_git(&repo, &["stash", "pop"]);

    assert_eq!(read_file(&repo, "stash.txt"), "base\nstashed\n");
    let stash_list = raw_git(&repo, &["stash", "list"]);
    assert!(
        stash_list.trim().is_empty(),
        "stash pop should drop the raw stash, got: {}",
        stash_list
    );
}

#[test]
fn test_cold_repo_traced_stash_after_raw_stash_history_preserves_current_ai_attribution() {
    let mut repo = cold_repo();
    raw_commit_file(&repo, "stash.txt", "base\n", "raw base");
    write_file(&repo, "stash.txt", "base\nold raw stash\n");
    raw_git(&repo, &["stash", "push"]);
    assert_eq!(read_file(&repo, "stash.txt"), "base\n");

    start_cold_daemon(&mut repo);
    write_file(&repo, "stash.txt", "base\ncurrent ai stash\n");
    repo.git_ai(&["checkpoint", "mock_ai", "stash.txt"])
        .unwrap_or_else(|error| panic!("mock_ai checkpoint failed: {}", error));
    run_traced_git_without_sync(&repo, &["stash", "push"]);
    assert_eq!(read_file(&repo, "stash.txt"), "base\n");

    run_traced_git_without_sync(&repo, &["stash", "pop"]);
    repo.stage_all_and_commit("apply current ai stash")
        .expect("apply current ai stash commit should succeed");

    let mut file = repo.filename("stash.txt");
    file.assert_lines_and_blame(crate::lines!["base".human(), "current ai stash".ai(),]);
}

crate::reuse_tests_in_worktree!(
    test_cold_repo_first_traced_commit_is_processed,
    test_cold_repo_commit_message_trailing_whitespace_preserves_ai_authorship,
    test_traced_commit_after_untraced_head_move_creates_authorship_note,
    test_traced_commit_after_untraced_duplicate_message_head_move_notes_traced_commit,
    test_cold_repo_first_traced_amend_is_processed,
    test_cold_repo_first_traced_soft_reset_is_processed,
    test_cold_repo_first_traced_rebase_is_processed,
    test_cold_repo_mid_rebase_continue_preserves_ai_conflict_resolution,
    test_cold_repo_mid_cherry_pick_continue_preserves_ai_conflict_resolution,
    test_cold_repo_mid_merge_commit_preserves_ai_conflict_resolution,
    test_cold_repo_first_traced_cherry_pick_is_processed,
    test_cold_repo_first_traced_squash_merge_is_processed,
    test_cold_daemon_first_traced_squash_merge_preserves_source_ai_authorship,
    test_cold_repo_first_traced_merge_is_processed,
    test_cold_repo_first_traced_stash_pop_is_processed,
    test_cold_repo_traced_stash_after_raw_stash_history_preserves_current_ai_attribution,
);
