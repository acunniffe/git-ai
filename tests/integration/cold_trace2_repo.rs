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

fn setup_pull_gap() -> (TestRepo, TestRepo, String, String) {
    let upstream = TestRepo::new_bare_with_daemon_scope(DaemonTestScope::NoDaemon);
    raw_git(&upstream, &["symbolic-ref", "HEAD", "refs/heads/main"]);

    let repo = TestRepo::new_dedicated_daemon();
    repo.git(&["branch", "-M", "main"]).unwrap();
    repo.git(&["remote", "add", "origin", upstream.path().to_str().unwrap()])
        .unwrap();
    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);
    repo.git(&["push", "-u", "origin", "HEAD:main"]).unwrap();

    repo.git(&["checkout", "-b", "missed-pull"]).unwrap();
    let missed_source = traced_ai_commit_file(
        &repo,
        "missed-pull.txt",
        "missed pull ai\n",
        "missed pull source",
    );
    let mut missed_file = repo.filename("missed-pull.txt");
    missed_file.assert_committed_lines(crate::lines!["missed pull ai".ai()]);

    repo.git(&["checkout", "main"]).unwrap();
    repo.git(&["checkout", "-b", "traced-pull"]).unwrap();
    let traced_source = traced_ai_commit_file(
        &repo,
        "traced-pull.txt",
        "traced pull ai\n",
        "traced pull source",
    );
    let mut traced_file = repo.filename("traced-pull.txt");
    traced_file.assert_committed_lines(crate::lines!["traced pull ai".ai()]);

    let contributor_parent = tempfile::tempdir().expect("contributor temp dir");
    let contributor_path = contributor_parent.path().join("contributor");
    let contributor = raw_clone(&upstream, &contributor_path);
    raw_git(&contributor, &["checkout", "main"]);
    raw_commit_file(
        &contributor,
        "upstream.txt",
        "upstream human\n",
        "upstream advance",
    );
    raw_git(&contributor, &["push", "origin", "HEAD:main"]);

    (upstream, repo, missed_source, traced_source)
}

#[test]
fn test_traced_pull_rebase_skips_prior_untraced_pull_rebase_span() {
    let (_upstream, repo, missed_source, traced_source) = setup_pull_gap();

    repo.git(&["checkout", "missed-pull"]).unwrap();
    repo.sync_daemon_force();
    raw_git(&repo, &["pull", "--rebase", "origin", "main"]);
    let missed_destination = raw_head(&repo);
    assert_ne!(missed_destination, missed_source);
    assert_no_authorship_note(&repo, &missed_destination);

    raw_git(&repo, &["checkout", "traced-pull"]);
    run_traced_git(&repo, &["pull", "--rebase", "origin", "main"]);
    let traced_destination = raw_head(&repo);
    assert_ne!(traced_destination, traced_source);
    let mut traced_file = repo.filename("traced-pull.txt");
    traced_file.assert_committed_lines(crate::lines!["traced pull ai".ai()]);
}

#[test]
fn test_traced_pull_merge_skips_prior_untraced_pull_merge_entry() {
    let (_upstream, repo, _missed_source, _traced_source) = setup_pull_gap();

    repo.git(&["checkout", "missed-pull"]).unwrap();
    repo.sync_daemon_force();
    raw_git(
        &repo,
        &["pull", "--no-rebase", "--no-edit", "origin", "main"],
    );
    assert_no_authorship_note(&repo, &raw_head(&repo));

    raw_git(&repo, &["checkout", "traced-pull"]);
    run_traced_git(
        &repo,
        &["pull", "--no-rebase", "--no-edit", "origin", "main"],
    );
    let mut traced_file = repo.filename("traced-pull.txt");
    traced_file.assert_committed_lines(crate::lines!["traced pull ai".ai()]);
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

    write_file(&repo, "next.txt", "next traced ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "next.txt"]).unwrap();
    repo.git(&["add", "next.txt"]).unwrap();
    run_traced_git(&repo, &["commit", "-m", "traced after raw"]);
    let traced_after_raw = raw_head(&repo);

    assert_ai_authorship_note(&repo, &traced_after_raw);
    let mut next = repo.filename("next.txt");
    next.assert_committed_lines(crate::lines!["next traced ai".ai()]);
}

#[test]
fn test_traced_reset_after_untraced_reset_preserves_recovered_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    write_file(&repo, "recovered.txt", "recovered ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "recovered.txt"])
        .unwrap();
    let recovered = repo
        .stage_all_and_commit("recovered source")
        .unwrap()
        .commit_sha;
    let mut recovered_file = repo.filename("recovered.txt");
    recovered_file.assert_committed_lines(crate::lines!["recovered ai".ai()]);

    write_file(&repo, "gap.txt", "gap ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "gap.txt"]).unwrap();
    repo.stage_all_and_commit("gap source").unwrap();
    let mut gap_file = repo.filename("gap.txt");
    gap_file.assert_committed_lines(crate::lines!["gap ai".ai()]);

    raw_git(&repo, &["reset", "--hard", &recovered]);
    run_traced_git(&repo, &["reset", "--mixed", &base]);
    repo.stage_all_and_commit("recommit recovered work")
        .unwrap();

    recovered_file.assert_committed_lines(crate::lines!["recovered ai".ai()]);
}

#[test]
fn test_traced_soft_reset_after_untraced_resets_reconstructs_multiple_commits() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    let first = traced_ai_commit_file(
        &repo,
        "first-reset.txt",
        "first reset ai\n",
        "first reset source",
    );
    let mut first_file = repo.filename("first-reset.txt");
    first_file.assert_committed_lines(crate::lines!["first reset ai".ai()]);
    let tip = traced_ai_commit_file(
        &repo,
        "second-reset.txt",
        "second reset ai\n",
        "second reset source",
    );
    let mut second_file = repo.filename("second-reset.txt");
    first_file.assert_committed_lines(crate::lines!["first reset ai".ai()]);
    second_file.assert_committed_lines(crate::lines!["second reset ai".ai()]);

    repo.sync_daemon_force();
    raw_git(&repo, &["reset", "--soft", &first]);
    raw_git(&repo, &["reset", "--hard", &tip]);

    run_traced_git(&repo, &["reset", "--soft", &base]);
    repo.stage_all_and_commit("recommit reset stack").unwrap();
    first_file.assert_committed_lines(crate::lines!["first reset ai".ai()]);
    second_file.assert_committed_lines(crate::lines!["second reset ai".ai()]);
}

#[test]
fn test_traced_cherry_pick_after_untraced_cherry_pick_preserves_source_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "pick-sources"]).unwrap();
    write_file(&repo, "missed-pick.txt", "missed pick ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "missed-pick.txt"])
        .unwrap();
    let missed_source = repo
        .stage_all_and_commit("missed pick source")
        .unwrap()
        .commit_sha;
    let mut missed_file = repo.filename("missed-pick.txt");
    missed_file.assert_committed_lines(crate::lines!["missed pick ai".ai()]);

    write_file(&repo, "traced-pick.txt", "traced pick ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "traced-pick.txt"])
        .unwrap();
    let traced_source = repo
        .stage_all_and_commit("traced pick source")
        .unwrap()
        .commit_sha;
    let mut traced_file = repo.filename("traced-pick.txt");
    traced_file.assert_committed_lines(crate::lines!["traced pick ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    write_file(&repo, "main-advance.txt", "main advance\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "main-advance.txt"])
        .unwrap();
    repo.stage_all_and_commit("main advance").unwrap();
    let mut main_advance = repo.filename("main-advance.txt");
    main_advance.assert_committed_lines(crate::lines!["main advance".human()]);
    repo.sync_daemon_force();
    raw_git(&repo, &["cherry-pick", &missed_source]);
    let missed_pick = raw_head(&repo);
    assert_no_authorship_note(&repo, &missed_pick);
    missed_file.assert_committed_lines(crate::lines!["missed pick ai".human()]);

    run_traced_git(&repo, &["cherry-pick", &traced_source]);
    missed_file.assert_committed_lines(crate::lines!["missed pick ai".human()]);
    traced_file.assert_committed_lines(crate::lines!["traced pick ai".ai()]);
}

#[test]
fn test_traced_multi_cherry_pick_skips_prior_untraced_multi_pick_span() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    repo.git(&["checkout", "-b", "missed-sources", &base])
        .unwrap();
    let missed_a = traced_ai_commit_file(&repo, "missed-a.txt", "missed a ai\n", "missed source a");
    repo.filename("missed-a.txt")
        .assert_committed_lines(crate::lines!["missed a ai".ai()]);
    let missed_b = traced_ai_commit_file(&repo, "missed-b.txt", "missed b ai\n", "missed source b");
    repo.filename("missed-a.txt")
        .assert_committed_lines(crate::lines!["missed a ai".ai()]);
    repo.filename("missed-b.txt")
        .assert_committed_lines(crate::lines!["missed b ai".ai()]);

    repo.git(&["checkout", "-b", "traced-sources", &base])
        .unwrap();
    let traced_a = traced_ai_commit_file(&repo, "traced-a.txt", "traced a ai\n", "traced source a");
    repo.filename("traced-a.txt")
        .assert_committed_lines(crate::lines!["traced a ai".ai()]);
    let traced_b = traced_ai_commit_file(&repo, "traced-b.txt", "traced b ai\n", "traced source b");
    repo.filename("traced-a.txt")
        .assert_committed_lines(crate::lines!["traced a ai".ai()]);
    repo.filename("traced-b.txt")
        .assert_committed_lines(crate::lines!["traced b ai".ai()]);

    repo.git(&["checkout", "-b", "destination", &base]).unwrap();
    repo.sync_daemon_force();
    raw_git(&repo, &["cherry-pick", &missed_a, &missed_b]);
    let missed_destinations = raw_git(&repo, &["rev-list", "--reverse", &format!("{base}..HEAD")]);
    for destination in missed_destinations.lines() {
        assert_no_authorship_note(&repo, destination);
    }

    raw_git(&repo, &["reset", "--hard", &base]);
    run_traced_git(&repo, &["cherry-pick", &traced_a, &traced_b]);
    repo.filename("traced-a.txt")
        .assert_committed_lines(crate::lines!["traced a ai".ai()]);
    repo.filename("traced-b.txt")
        .assert_committed_lines(crate::lines!["traced b ai".ai()]);
}

#[test]
fn test_traced_revert_after_untraced_revert_restores_source_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();
    let missed_path = "missed-revert.txt";
    let traced_path = "traced-revert.txt";

    write_file(&repo, missed_path, "missed ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", missed_path])
        .unwrap();
    write_file(&repo, traced_path, "traced ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", traced_path])
        .unwrap();
    repo.stage_all_and_commit("ai source lines").unwrap();
    let mut missed_file = repo.filename(missed_path);
    missed_file.assert_committed_lines(crate::lines!["missed ai".ai()]);
    let mut traced_file = repo.filename(traced_path);
    traced_file.assert_committed_lines(crate::lines!["traced ai".ai()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "missed-revert-source"])
        .unwrap();
    fs::remove_file(repo.path().join(missed_path)).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", missed_path])
        .unwrap();
    let missed_delete = repo
        .stage_all_and_commit("delete missed file")
        .unwrap()
        .commit_sha;
    assert!(!repo.path().join(missed_path).exists());
    traced_file.assert_committed_lines(crate::lines!["traced ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["checkout", "-b", "traced-revert-source"])
        .unwrap();
    fs::remove_file(repo.path().join(traced_path)).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", traced_path])
        .unwrap();
    let traced_delete = repo
        .stage_all_and_commit("delete traced file")
        .unwrap()
        .commit_sha;
    assert!(!repo.path().join(traced_path).exists());
    missed_file.assert_committed_lines(crate::lines!["missed ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    fs::remove_file(repo.path().join(missed_path)).unwrap();
    fs::remove_file(repo.path().join(traced_path)).unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", missed_path])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", traced_path])
        .unwrap();
    repo.stage_all_and_commit("delete both files").unwrap();
    assert!(!repo.path().join(missed_path).exists());
    assert!(!repo.path().join(traced_path).exists());

    raw_git(&repo, &["revert", "--no-edit", &missed_delete]);
    let missed_revert = raw_head(&repo);
    assert_no_authorship_note(&repo, &missed_revert);
    missed_file.assert_committed_lines(crate::lines!["missed ai".human()]);

    run_traced_git(&repo, &["revert", "--no-edit", &traced_delete]);
    missed_file.assert_committed_lines(crate::lines!["missed ai".human()]);
    traced_file.assert_committed_lines(crate::lines!["traced ai".ai()]);
}

#[test]
fn test_traced_multi_revert_after_untraced_multi_revert_restores_each_source() {
    let repo = TestRepo::new_dedicated_daemon();
    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let main = repo.current_branch();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    let make_delete_source = |branch: &str, path: &str, message: &str| {
        repo.git(&["checkout", "-b", branch, &base]).unwrap();
        traced_ai_commit_file(&repo, path, &format!("{path} ai\n"), message);
        repo.filename(path)
            .assert_committed_lines(crate::lines![format!("{path} ai").ai()]);
        fs::remove_file(repo.path().join(path)).unwrap();
        repo.git_ai(&["checkpoint", "mock_known_human", path])
            .unwrap();
        let deleted = repo
            .stage_all_and_commit(&format!("delete {path}"))
            .unwrap()
            .commit_sha;
        assert!(!repo.path().join(path).exists());
        deleted
    };

    let missed_a = make_delete_source("missed-a-source", "missed-a.txt", "add missed a");
    let missed_b = make_delete_source("missed-b-source", "missed-b.txt", "add missed b");
    let traced_a = make_delete_source("traced-a-source", "traced-a.txt", "add traced a");
    let traced_b = make_delete_source("traced-b-source", "traced-b.txt", "add traced b");

    repo.git(&["checkout", &main]).unwrap();
    let deleted_all = raw_head(&repo);

    repo.sync_daemon_force();
    raw_git(&repo, &["revert", "--no-edit", &missed_a, &missed_b]);
    let missed_destinations = raw_git(
        &repo,
        &["rev-list", "--reverse", &format!("{deleted_all}..HEAD")],
    );
    for destination in missed_destinations.lines() {
        assert_no_authorship_note(&repo, destination);
    }
    raw_git(&repo, &["reset", "--hard", &deleted_all]);

    run_traced_git(&repo, &["revert", "--no-edit", &traced_a, &traced_b]);
    repo.filename("traced-a.txt")
        .assert_committed_lines(crate::lines!["traced-a.txt ai".ai()]);
    repo.filename("traced-b.txt")
        .assert_committed_lines(crate::lines!["traced-b.txt ai".ai()]);
    assert!(!repo.path().join("missed-a.txt").exists());
    assert!(!repo.path().join("missed-b.txt").exists());
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
fn test_traced_amend_after_untraced_amend_preserves_existing_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "missed-amend"]).unwrap();
    write_file(&repo, "missed-amend.txt", "missed amend ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "missed-amend.txt"])
        .unwrap();
    repo.stage_all_and_commit("missed amend source").unwrap();
    let mut missed_file = repo.filename("missed-amend.txt");
    missed_file.assert_committed_lines(crate::lines!["missed amend ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["checkout", "-b", "traced-amend"]).unwrap();
    write_file(&repo, "traced-amend.txt", "traced amend ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "traced-amend.txt"])
        .unwrap();
    repo.stage_all_and_commit("traced amend source").unwrap();
    let mut traced_file = repo.filename("traced-amend.txt");
    traced_file.assert_committed_lines(crate::lines!["traced amend ai".ai()]);

    repo.git(&["checkout", "missed-amend"]).unwrap();
    repo.sync_daemon_force();
    raw_git(
        &repo,
        &["commit", "--amend", "-m", "missed amend destination"],
    );
    let missed_amend = raw_head(&repo);
    assert_no_authorship_note(&repo, &missed_amend);
    raw_git(&repo, &["checkout", "traced-amend"]);

    run_traced_git(
        &repo,
        &["commit", "--amend", "-m", "traced amend destination"],
    );
    traced_file.assert_committed_lines(crate::lines!["traced amend ai".ai()]);
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
fn test_traced_rebase_after_untraced_rebase_preserves_source_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "missed-rebase"]).unwrap();
    write_file(&repo, "missed-rebase.txt", "missed rebase ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "missed-rebase.txt"])
        .unwrap();
    repo.stage_all_and_commit("missed rebase source").unwrap();
    let mut missed_file = repo.filename("missed-rebase.txt");
    missed_file.assert_committed_lines(crate::lines!["missed rebase ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["checkout", "-b", "traced-rebase"]).unwrap();
    write_file(&repo, "traced-rebase.txt", "traced rebase ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "traced-rebase.txt"])
        .unwrap();
    repo.stage_all_and_commit("traced rebase source").unwrap();
    let mut traced_file = repo.filename("traced-rebase.txt");
    traced_file.assert_committed_lines(crate::lines!["traced rebase ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    write_file(&repo, "main-advance.txt", "main advance\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "main-advance.txt"])
        .unwrap();
    repo.stage_all_and_commit("main advance").unwrap();
    let mut main_advance = repo.filename("main-advance.txt");
    main_advance.assert_committed_lines(crate::lines!["main advance".human()]);

    repo.git(&["checkout", "missed-rebase"]).unwrap();
    repo.sync_daemon_force();
    raw_git(&repo, &["rebase", &main]);
    let missed_rebase = raw_head(&repo);
    assert_no_authorship_note(&repo, &missed_rebase);
    raw_git(&repo, &["checkout", "traced-rebase"]);

    run_traced_git(&repo, &["rebase", &main]);
    traced_file.assert_committed_lines(crate::lines!["traced rebase ai".ai()]);
}

#[test]
fn test_traced_multi_commit_rebase_after_untraced_rebase_preserves_rename_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let main = repo.current_branch();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    repo.git(&["checkout", "-b", "missed-multi", &base])
        .unwrap();
    traced_ai_commit_file(
        &repo,
        "missed-one.txt",
        "missed one ai\n",
        "missed multi one",
    );
    repo.filename("missed-one.txt")
        .assert_committed_lines(crate::lines!["missed one ai".ai()]);
    traced_ai_commit_file(
        &repo,
        "missed-two.txt",
        "missed two ai\n",
        "missed multi two",
    );
    repo.filename("missed-one.txt")
        .assert_committed_lines(crate::lines!["missed one ai".ai()]);
    repo.filename("missed-two.txt")
        .assert_committed_lines(crate::lines!["missed two ai".ai()]);

    repo.git(&["checkout", "-b", "traced-multi", &base])
        .unwrap();
    traced_ai_commit_file(
        &repo,
        "before-rename.txt",
        "renamed ai\n",
        "traced multi one",
    );
    repo.filename("before-rename.txt")
        .assert_committed_lines(crate::lines!["renamed ai".ai()]);
    repo.git(&["mv", "before-rename.txt", "after-rename.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "after-rename.txt"])
        .unwrap();
    repo.stage_all_and_commit("traced multi rename").unwrap();
    let mut renamed = repo.filename("after-rename.txt");
    renamed.assert_committed_lines(crate::lines!["renamed ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    write_file(&repo, "main-advance.txt", "main human\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "main-advance.txt"])
        .unwrap();
    repo.stage_all_and_commit("main advance").unwrap();
    repo.filename("main-advance.txt")
        .assert_committed_lines(crate::lines!["main human".human()]);

    repo.git(&["checkout", "missed-multi"]).unwrap();
    repo.sync_daemon_force();
    raw_git(&repo, &["rebase", &main]);
    assert_no_authorship_note(&repo, &raw_head(&repo));
    raw_git(&repo, &["checkout", "traced-multi"]);

    run_traced_git(&repo, &["rebase", &main]);
    renamed.assert_committed_lines(crate::lines!["renamed ai".ai()]);
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
fn test_traced_squash_merge_after_untraced_squash_merge_preserves_source_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();

    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);
    let main = repo.current_branch();

    repo.git(&["checkout", "-b", "missed-squash"]).unwrap();
    write_file(&repo, "missed-squash.txt", "missed squash ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "missed-squash.txt"])
        .unwrap();
    repo.stage_all_and_commit("missed squash source").unwrap();
    let mut missed_file = repo.filename("missed-squash.txt");
    missed_file.assert_committed_lines(crate::lines!["missed squash ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.git(&["checkout", "-b", "traced-squash"]).unwrap();
    write_file(&repo, "traced-squash.txt", "traced squash ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "traced-squash.txt"])
        .unwrap();
    repo.stage_all_and_commit("traced squash source").unwrap();
    let mut traced_file = repo.filename("traced-squash.txt");
    traced_file.assert_committed_lines(crate::lines!["traced squash ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    repo.sync_daemon_force();
    raw_git(&repo, &["merge", "--squash", "missed-squash"]);
    raw_git(&repo, &["commit", "-m", "missed squash destination"]);
    let missed_squash = raw_head(&repo);
    assert_no_authorship_note(&repo, &missed_squash);

    run_traced_git_without_sync(&repo, &["merge", "--squash", "traced-squash"]);
    run_traced_git(&repo, &["commit", "-m", "traced squash destination"]);
    missed_file.assert_committed_lines(crate::lines!["missed squash ai".human()]);
    traced_file.assert_committed_lines(crate::lines!["traced squash ai".ai()]);
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
fn test_traced_merge_after_untraced_merge_preserves_both_source_attributions() {
    let repo = TestRepo::new_dedicated_daemon();
    write_file(&repo, "base.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "base.txt"])
        .unwrap();
    let base = repo.stage_all_and_commit("base").unwrap().commit_sha;
    let main = repo.current_branch();
    let mut base_file = repo.filename("base.txt");
    base_file.assert_committed_lines(crate::lines!["base".human()]);

    repo.git(&["checkout", "-b", "missed-merge", &base])
        .unwrap();
    traced_ai_commit_file(
        &repo,
        "missed-merge.txt",
        "missed merge ai\n",
        "missed merge source",
    );
    repo.filename("missed-merge.txt")
        .assert_committed_lines(crate::lines!["missed merge ai".ai()]);

    repo.git(&["checkout", "-b", "traced-merge", &base])
        .unwrap();
    traced_ai_commit_file(
        &repo,
        "traced-merge.txt",
        "traced merge ai\n",
        "traced merge source",
    );
    let mut traced_file = repo.filename("traced-merge.txt");
    traced_file.assert_committed_lines(crate::lines!["traced merge ai".ai()]);

    repo.git(&["checkout", &main]).unwrap();
    write_file(&repo, "main.txt", "main human\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "main.txt"])
        .unwrap();
    repo.stage_all_and_commit("main advance").unwrap();
    repo.filename("main.txt")
        .assert_committed_lines(crate::lines!["main human".human()]);

    repo.sync_daemon_force();
    raw_git(
        &repo,
        &["merge", "--no-ff", "missed-merge", "-m", "missed merge"],
    );
    assert_no_authorship_note(&repo, &raw_head(&repo));
    run_traced_git(
        &repo,
        &["merge", "--no-ff", "traced-merge", "-m", "traced merge"],
    );

    repo.filename("missed-merge.txt")
        .assert_committed_lines(crate::lines!["missed merge ai".ai()]);
    traced_file.assert_committed_lines(crate::lines!["traced merge ai".ai()]);
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
fn test_traced_stash_after_untraced_stash_preserves_current_ai_attribution() {
    let repo = TestRepo::new_dedicated_daemon();
    write_file(&repo, "stash.txt", "base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "stash.txt"])
        .unwrap();
    repo.stage_all_and_commit("base").unwrap();
    let mut file = repo.filename("stash.txt");
    file.assert_committed_lines(crate::lines!["base".human()]);

    write_file(&repo, "stash.txt", "base\nold raw stash\n");
    raw_git(&repo, &["stash", "push"]);
    assert_eq!(read_file(&repo, "stash.txt"), "base\n");

    write_file(&repo, "stash.txt", "base\ncurrent ai stash\n");
    repo.git_ai(&["checkpoint", "mock_ai", "stash.txt"])
        .unwrap_or_else(|error| panic!("mock_ai checkpoint failed: {}", error));
    run_traced_git_without_sync(&repo, &["stash", "push"]);
    assert_eq!(read_file(&repo, "stash.txt"), "base\n");

    run_traced_git_without_sync(&repo, &["stash", "pop"]);
    repo.stage_all_and_commit("apply current ai stash")
        .expect("apply current ai stash commit should succeed");

    file.assert_committed_lines(crate::lines!["base".human(), "current ai stash".ai(),]);
}

#[test]
fn test_traced_symbolic_stash_apply_after_untraced_drop_uses_current_stack() {
    let repo = TestRepo::new_dedicated_daemon();
    write_file(&repo, "first.txt", "first base\n");
    write_file(&repo, "second.txt", "second base\n");
    repo.git_ai(&["checkpoint", "mock_known_human", "first.txt"])
        .unwrap();
    repo.git_ai(&["checkpoint", "mock_known_human", "second.txt"])
        .unwrap();
    repo.stage_all_and_commit("stash base").unwrap();
    let mut first = repo.filename("first.txt");
    let mut second = repo.filename("second.txt");
    first.assert_committed_lines(crate::lines!["first base".human()]);
    second.assert_committed_lines(crate::lines!["second base".human()]);

    write_file(&repo, "first.txt", "first base\nfirst stash ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "first.txt"])
        .unwrap();
    run_traced_git(&repo, &["stash", "push", "-m", "first stash"]);

    write_file(&repo, "second.txt", "second base\nsecond stash ai\n");
    repo.git_ai(&["checkpoint", "mock_ai", "second.txt"])
        .unwrap();
    run_traced_git(&repo, &["stash", "push", "-m", "second stash"]);

    repo.sync_daemon_force();
    raw_git(&repo, &["stash", "drop", "stash@{0}"]);
    run_traced_git(&repo, &["stash", "apply", "stash@{0}"]);
    repo.stage_all_and_commit("apply surviving stash").unwrap();

    first.assert_committed_lines(crate::lines!["first base".human(), "first stash ai".ai()]);
    second.assert_committed_lines(crate::lines!["second base".human()]);
}

crate::reuse_tests_in_worktree!(
    test_cold_repo_first_traced_commit_is_processed,
    test_cold_repo_commit_message_trailing_whitespace_preserves_ai_authorship,
    test_traced_pull_rebase_skips_prior_untraced_pull_rebase_span,
    test_traced_pull_merge_skips_prior_untraced_pull_merge_entry,
    test_traced_commit_after_untraced_head_move_creates_authorship_note,
    test_traced_reset_after_untraced_reset_preserves_recovered_ai_attribution,
    test_traced_soft_reset_after_untraced_resets_reconstructs_multiple_commits,
    test_traced_cherry_pick_after_untraced_cherry_pick_preserves_source_ai_attribution,
    test_traced_multi_cherry_pick_skips_prior_untraced_multi_pick_span,
    test_traced_revert_after_untraced_revert_restores_source_ai_attribution,
    test_traced_multi_revert_after_untraced_multi_revert_restores_each_source,
    test_traced_commit_after_untraced_duplicate_message_head_move_notes_traced_commit,
    test_cold_repo_first_traced_amend_is_processed,
    test_traced_amend_after_untraced_amend_preserves_existing_ai_attribution,
    test_cold_repo_first_traced_soft_reset_is_processed,
    test_cold_repo_first_traced_rebase_is_processed,
    test_traced_rebase_after_untraced_rebase_preserves_source_ai_attribution,
    test_traced_multi_commit_rebase_after_untraced_rebase_preserves_rename_attribution,
    test_cold_repo_mid_rebase_continue_preserves_ai_conflict_resolution,
    test_cold_repo_mid_cherry_pick_continue_preserves_ai_conflict_resolution,
    test_cold_repo_mid_merge_commit_preserves_ai_conflict_resolution,
    test_cold_repo_first_traced_cherry_pick_is_processed,
    test_cold_repo_first_traced_squash_merge_is_processed,
    test_traced_squash_merge_after_untraced_squash_merge_preserves_source_ai_attribution,
    test_cold_daemon_first_traced_squash_merge_preserves_source_ai_authorship,
    test_cold_repo_first_traced_merge_is_processed,
    test_traced_merge_after_untraced_merge_preserves_both_source_attributions,
    test_cold_repo_first_traced_stash_pop_is_processed,
    test_traced_stash_after_untraced_stash_preserves_current_ai_attribution,
    test_traced_symbolic_stash_apply_after_untraced_drop_uses_current_stack,
);
