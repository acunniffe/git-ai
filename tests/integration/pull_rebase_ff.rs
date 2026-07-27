use git_ai::authorship::authorship_log_serialization::AuthorshipLog;
use git_ai::authorship::working_log::AgentId;
use git_ai::daemon::bash_history_db::{BashCallEnd, BashCallStart, BashHistoryDatabase};

use crate::repos::test_file::ExpectedLineExt;
use crate::repos::test_repo::{DaemonTestScope, TestRepo};
use serde_json::json;
use std::collections::{BTreeSet, HashMap};
use std::time::{SystemTime, UNIX_EPOCH};

/// Helper struct that provides a local repo with an upstream containing seeded commits.
/// The local repo is initially behind the upstream (no divergence — fast-forward possible).
struct PullTestSetup {
    /// The local clone - initially behind upstream after setup
    local: TestRepo,
    /// The bare upstream repository (kept alive for the duration of the test)
    #[allow(dead_code)]
    upstream: TestRepo,
    /// SHA of the second commit (upstream is ahead by this)
    upstream_sha: String,
}

/// Creates a test setup for fast-forward pull scenarios:
/// 1. Creates upstream (bare) and local (clone) repos
/// 2. Makes an initial commit in local, pushes to upstream
/// 3. Makes a second commit in local, pushes to upstream
/// 4. Resets local back to initial commit (so local is behind upstream)
///
/// After this setup:
/// - upstream has 2 commits
/// - local has 1 commit (behind by 1)
/// - local can `git pull` to fast-forward to the second commit
fn setup_pull_test() -> PullTestSetup {
    let (local, upstream) = TestRepo::new_with_remote();

    // Make initial commit in local and push
    let mut readme = local.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    let commit = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");

    let initial_sha = commit.commit_sha;

    // Push initial commit to upstream
    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    // Make second commit (simulating remote changes)
    let mut file = local.filename("upstream_file.txt");
    file.set_contents(vec!["content from upstream".to_string()]);
    let commit = local
        .stage_all_and_commit("upstream commit")
        .expect("upstream commit should succeed");

    let upstream_sha = commit.commit_sha;

    // Push second commit to upstream
    local
        .git(&["push", "origin", "HEAD"])
        .expect("push upstream commit should succeed");

    // Reset local back to initial commit (so it's behind upstream)
    local
        .git(&["reset", "--hard", &initial_sha])
        .expect("reset to initial commit should succeed");

    // Verify local is behind
    assert!(
        local.read_file("upstream_file.txt").is_none(),
        "Local should not have upstream_file.txt after reset"
    );

    PullTestSetup {
        local,
        upstream,
        upstream_sha,
    }
}

/// Helper struct for divergent pull scenarios where local has committed changes
/// and upstream has diverged, requiring a real rebase (not fast-forward).
struct DivergentPullTestSetup {
    local: TestRepo,
    #[allow(dead_code)]
    upstream: TestRepo,
    /// SHA of the local AI commit (will get a new SHA after rebase)
    local_ai_commit_sha: String,
}

/// Creates a test setup for divergent pull --rebase scenarios:
/// 1. Creates upstream (bare) and local (clone) repos
/// 2. Makes an initial commit, pushes to upstream
/// 3. Makes a local AI-authored commit
/// 4. Creates a divergent upstream commit (force-pushed)
/// 5. Resets local back to the AI commit
///
/// After this setup:
/// - upstream has diverged from local (initial + upstream_commit)
/// - local has diverged from upstream (initial + ai_commit)
/// - `git pull --rebase` will rebase the AI commit onto the upstream commit
fn setup_divergent_pull_test() -> DivergentPullTestSetup {
    let (local, upstream) = TestRepo::new_with_remote();

    // Make initial commit and push
    let mut readme = local.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    // Create a local committed AI-authored change
    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.set_contents(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    local
        .stage_all_and_commit("add AI feature")
        .expect("AI feature commit should succeed");

    let ai_commit_sha = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    let branch = local.current_branch();

    // Create a divergent upstream commit: reset to initial, commit, force-push
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset should succeed");

    let mut upstream_file = local.filename("upstream_change.txt");
    upstream_file.set_contents(vec!["upstream content".to_string()]);
    local
        .stage_all_and_commit("upstream divergent commit")
        .expect("upstream commit should succeed");

    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("force push upstream commit should succeed");

    // Reset back to the local AI commit
    local
        .git(&["reset", "--hard", &ai_commit_sha])
        .expect("reset to AI commit should succeed");

    DivergentPullTestSetup {
        local,
        upstream,
        local_ai_commit_sha: ai_commit_sha,
    }
}

/// Creates a setup where local has one AI commit and upstream has an equivalent patch
/// under a different commit hash plus additional upstream commits.
/// A subsequent `pull --rebase` should skip the local commit and not map all upstream history
/// as "new rebased commits".
fn setup_pull_rebase_skip_test() -> (TestRepo, TestRepo, String) {
    let (local, upstream) = TestRepo::new_with_remote();

    // Initial commit and push
    let mut readme = local.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    // Local AI commit (this is the one that should be skipped during pull --rebase)
    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.set_contents(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    let local_ai = local
        .stage_all_and_commit("local ai commit")
        .expect("local ai commit should succeed");

    let branch = local.current_branch();

    // Simulate upstream history with equivalent patch under a different commit hash.
    // Reset to initial, re-commit same file content with different message, then add extra commits.
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial should succeed");

    ai_file.set_contents(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    local
        .stage_all_and_commit("upstream equivalent ai commit")
        .expect("upstream equivalent ai commit should succeed");

    let mut upstream_file = local.filename("upstream_only.txt");
    upstream_file.set_contents(vec!["upstream extra 1".to_string()]);
    local
        .stage_all_and_commit("upstream extra 1")
        .expect("upstream extra 1 should succeed");
    upstream_file.set_contents(vec![
        "upstream extra 1".to_string(),
        "upstream extra 2".to_string(),
    ]);
    local
        .stage_all_and_commit("upstream extra 2")
        .expect("upstream extra 2 should succeed");

    // Force-push divergent upstream state
    local
        .git(&["push", "--force", "origin", &format!("HEAD:{}", branch)])
        .expect("force push upstream state should succeed");

    // Restore local branch to the original local AI commit (now divergent from upstream)
    local
        .git(&["reset", "--hard", &local_ai.commit_sha])
        .expect("reset back to local ai commit should succeed");

    (local, upstream, local_ai.commit_sha)
}

fn isolated_bash_history_db_path() -> (tempfile::TempDir, String) {
    let dir = tempfile::tempdir().expect("failed to create isolated bash history db dir");
    let path = dir.path().join("bash-history.db");
    (dir, path.to_string_lossy().to_string())
}

fn insert_bash_recovery_call_covering_now(db_path: &str, repo: &TestRepo) {
    let now_ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system time should be after epoch")
        .as_nanos();
    let started_at_ns = now_ns.saturating_sub(1_000_000_000);
    let ended_at_ns = now_ns.saturating_add(10_000_000_000);
    let repo_work_dir = repo.canonical_path().to_string_lossy().to_string();
    let agent_id = AgentId {
        tool: "codex".to_string(),
        id: "fast-forward-recovery-session".to_string(),
        model: "gpt-5".to_string(),
    };

    let mut db = BashHistoryDatabase::open_at_path(std::path::Path::new(db_path))
        .expect("bash history db should open");
    db.record_start(&BashCallStart {
        original_cwd: repo_work_dir.clone(),
        repo_work_dir: Some(repo_work_dir.clone()),
        repo_discovery_error: None,
        session_id: agent_id.id.clone(),
        tool_use_id: "fast-forward-recovery-tool-use".to_string(),
        agent_id: agent_id.clone(),
        start_trace_id: "fast-forward-recovery-start".to_string(),
        started_at_ns,
        command: Some("codex exec".to_string()),
        metadata: HashMap::new(),
    })
    .expect("bash call start should insert");
    db.record_end(&BashCallEnd {
        original_cwd: repo_work_dir.clone(),
        repo_work_dir: Some(repo_work_dir),
        repo_discovery_error: None,
        session_id: agent_id.id.clone(),
        tool_use_id: "fast-forward-recovery-tool-use".to_string(),
        agent_id,
        start_trace_id: Some("fast-forward-recovery-start".to_string()),
        end_trace_id: "fast-forward-recovery-end".to_string(),
        started_at_ns: Some(started_at_ns),
        ended_at_ns,
        command: Some("codex exec".to_string()),
        metadata: HashMap::new(),
    })
    .expect("bash call end should insert");
}

// =============================================================================
// Fast-forward pull tests
// =============================================================================

#[test]
fn test_fast_forward_pull_preserves_ai_attribution() {
    let setup = setup_pull_test();
    let local = setup.local;

    // Create local AI changes (uncommitted)
    let mut ai_file = local.filename("ai_work.txt");
    ai_file.set_contents(vec!["AI generated line 1".ai(), "AI generated line 2".ai()]);

    local
        .git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Configure git pull behavior for Git 2.52.0+ compatibility
    local
        .git(&["config", "pull.rebase", "false"])
        .expect("config should succeed");
    local
        .git(&["config", "pull.ff", "only"])
        .expect("config should succeed");

    // Perform fast-forward pull
    local.git(&["pull"]).expect("pull should succeed");

    // Commit and verify AI attribution is preserved through the ff pull
    local
        .stage_all_and_commit("commit after pull")
        .expect("commit should succeed");
    ai_file.assert_lines_and_blame(vec!["AI generated line 1".ai(), "AI generated line 2".ai()]);
}

#[test]
fn test_fast_forward_pull_without_local_changes() {
    let setup = setup_pull_test();
    let local = setup.local;

    // Configure git pull behavior
    local
        .git(&["config", "pull.ff", "only"])
        .expect("config should succeed");

    // No local changes - just a clean fast-forward pull
    local.git(&["pull"]).expect("pull should succeed");

    // Verify we got the upstream changes
    assert!(
        local.read_file("upstream_file.txt").is_some(),
        "Should have upstream_file.txt after pull"
    );

    // Verify HEAD is at the expected upstream commit
    let head = local.git(&["rev-parse", "HEAD"]).unwrap();
    assert_eq!(
        head.trim(),
        setup.upstream_sha,
        "HEAD should be at upstream commit"
    );
}

#[test]
fn test_fast_forward_update_ref_bounds_recovery_to_new_tip_parent() {
    let (_bash_db_dir, bash_db_path) = isolated_bash_history_db_path();
    let local = TestRepo::new_with_daemon_env(&[(
        "GIT_AI_TEST_BASH_CHECKPOINT_DB_PATH",
        bash_db_path.as_str(),
    )]);
    let upstream_dir = tempfile::tempdir().expect("upstream temp dir");
    let upstream_path = upstream_dir.path().join("upstream.git");
    let upstream = upstream_path.to_string_lossy().to_string();

    local
        .git_og(&["init", "--bare", &upstream])
        .expect("bare upstream init should succeed");
    local
        .git(&["remote", "add", "origin", &upstream])
        .expect("remote add should succeed");

    std::fs::write(local.path().join("base.txt"), "base\n").expect("write base");
    let old_tip = local
        .stage_all_and_commit("old tip")
        .expect("old tip commit should succeed")
        .commit_sha;
    let branch = local.current_branch();
    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push old tip should succeed");
    local
        .git_og(&[
            "--git-dir",
            &upstream,
            "symbolic-ref",
            "HEAD",
            &format!("refs/heads/{branch}"),
        ])
        .expect("set upstream HEAD should succeed");

    // Seed a working log at the old local tip. The daemon fast-forward
    // update-ref path only finalizes attribution when such a log exists.
    std::fs::write(local.path().join("local_draft.txt"), "local draft\n")
        .expect("write local draft");
    local
        .git_ai(&["checkpoint", "mock_ai", "local_draft.txt"])
        .expect("local checkpoint should succeed");

    let contributor_dir = tempfile::tempdir().expect("contributor temp dir");
    let contributor_path = contributor_dir.path().join("contributor");
    local
        .git_og(&[
            "clone",
            &upstream,
            contributor_path
                .to_str()
                .expect("contributor path should be utf-8"),
        ])
        .expect("contributor clone should succeed");
    let contributor =
        TestRepo::new_at_path_with_daemon_scope(&contributor_path, DaemonTestScope::NoDaemon);

    std::fs::write(
        contributor.path().join("pulled_early_1.txt"),
        "pulled early 1\n",
    )
    .expect("write pulled early 1");
    contributor.git_og(&["add", "-A"]).unwrap();
    contributor
        .git_og(&["commit", "-m", "pulled early 1"])
        .expect("commit pulled early 1 should succeed");
    std::fs::write(
        contributor.path().join("pulled_early_2.txt"),
        "pulled early 2\n",
    )
    .expect("write pulled early 2");
    contributor.git_og(&["add", "-A"]).unwrap();
    contributor
        .git_og(&["commit", "-m", "pulled early 2"])
        .expect("commit pulled early 2 should succeed");
    std::fs::write(contributor.path().join("final_tip.txt"), "final tip\n")
        .expect("write final tip");
    contributor.git_og(&["add", "-A"]).unwrap();
    contributor
        .git_og(&["commit", "-m", "final tip"])
        .expect("commit final tip should succeed");
    contributor
        .git_og(&["push", "origin", &format!("HEAD:{branch}")])
        .expect("push contributor range should succeed");

    local
        .git(&["fetch", "origin", &branch])
        .expect("fetch contributor range should succeed");
    let new_tip = local
        .git(&["rev-parse", "FETCH_HEAD"])
        .expect("rev-parse FETCH_HEAD should succeed")
        .trim()
        .to_string();
    let contributor_final = contributor
        .git_og(&["rev-parse", "HEAD"])
        .expect("rev-parse contributor HEAD should succeed")
        .trim()
        .to_string();
    assert_eq!(new_tip, contributor_final);
    assert_ne!(old_tip, new_tip);

    // `git update-ref` moves the ref but does not update the worktree. Put the
    // files in the worktree so timestamp-based recovery can run deterministically
    // and prove which committed hunks it was allowed to inspect.
    std::fs::write(local.path().join("pulled_early_1.txt"), "pulled early 1\n")
        .expect("write local pulled early 1");
    std::fs::write(local.path().join("pulled_early_2.txt"), "pulled early 2\n")
        .expect("write local pulled early 2");
    std::fs::write(local.path().join("final_tip.txt"), "final tip\n")
        .expect("write local final tip");

    insert_bash_recovery_call_covering_now(&bash_db_path, &local);
    local
        .git(&[
            "update-ref",
            &format!("refs/heads/{branch}"),
            &new_tip,
            &old_tip,
        ])
        .expect("fast-forward update-ref should succeed");

    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse HEAD should succeed")
        .trim()
        .to_string();
    assert_eq!(new_head, new_tip);

    let note = local
        .read_authorship_note(&new_head)
        .expect("fast-forward update-ref finalization should write an authorship note");
    let log =
        AuthorshipLog::deserialize_from_string(&note).expect("authorship note should deserialize");
    let attested_files: BTreeSet<String> = log
        .attestations
        .iter()
        .map(|attestation| attestation.file_path.clone())
        .collect();

    assert!(
        attested_files.contains("final_tip.txt"),
        "recovery should still see the finalized tip commit"
    );
    assert!(
        !attested_files.contains("pulled_early_1.txt"),
        "recovery must not diff the whole old_tip..new_head range"
    );
    assert!(
        !attested_files.contains("pulled_early_2.txt"),
        "recovery must not attribute earlier pulled commits to the local session"
    );
}

// =============================================================================
// Pull --rebase with committed changes (the core bug fix)
// =============================================================================

#[test]
fn test_pull_rebase_preserves_committed_ai_authorship() {
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Perform pull --rebase (committed local changes will be rebased onto upstream)
    local
        .git(&["pull", "--rebase"])
        .expect("pull --rebase should succeed");

    // Verify we got upstream changes
    assert!(
        local.read_file("upstream_change.txt").is_some(),
        "Should have upstream_change.txt after pull --rebase"
    );

    // The AI commit got a new SHA after rebase
    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.local_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    // Verify AI authorship is preserved on the rebased commit
    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
}

#[test]
fn test_pull_rebase_force_pushed_target_preserves_remote_authorship_note() {
    // Model a clone that still has an old PR head while another clone force-pushes
    // a rewritten head with its own complete authorship note. Pull processes the
    // non-fast-forward branch update before its post-pull notes fetch, so rewrite
    // handling must fetch the target note before shifting the stale source note.
    let (local, upstream) = TestRepo::new_with_remote();
    let file_path = local.path().join("feature.txt");

    std::fs::write(&file_path, "base\n").unwrap();
    local.stage_all_and_commit("initial").unwrap();
    let mut local_file = local.filename("feature.txt");
    local_file.assert_committed_lines(crate::lines!["base".unattributed_human()]);

    local
        .git_ai(&["checkpoint", "human", "feature.txt"])
        .unwrap();
    std::fs::write(&file_path, "base\nold AI line\n").unwrap();
    local
        .git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    let old_commit = local.stage_all_and_commit("old feature version").unwrap();
    local_file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "old AI line".ai(),
    ]);
    local.git(&["push", "-u", "origin", "main"]).unwrap();

    let contributor_parent = tempfile::tempdir().expect("contributor temp dir");
    let contributor_path = contributor_parent.path().join("contributor");
    local
        .git_og(&[
            "clone",
            upstream.path().to_str().unwrap(),
            contributor_path.to_str().unwrap(),
        ])
        .unwrap();
    let contributor =
        TestRepo::new_at_path_with_daemon_scope(&contributor_path, DaemonTestScope::Shared);
    contributor
        .git(&["reset", "--hard", &format!("{}^", old_commit.commit_sha)])
        .unwrap();

    let contributor_file_path = contributor.path().join("feature.txt");
    contributor
        .git_ai(&["checkpoint", "human", "feature.txt"])
        .unwrap();
    std::fs::write(
        &contributor_file_path,
        "base\nold AI line\nremote AI line\n",
    )
    .unwrap();
    contributor
        .git_ai(&["checkpoint", "mock_ai", "feature.txt"])
        .unwrap();
    let remote_commit = contributor
        .stage_all_and_commit("force-pushed feature version")
        .unwrap();
    let mut contributor_file = contributor.filename("feature.txt");
    contributor_file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "old AI line".ai(),
        "remote AI line".ai(),
    ]);

    contributor
        .git(&["push", "--force", "origin", "HEAD:main"])
        .unwrap();
    contributor
        .git_og(&["push", "--force", "origin", "refs/notes/ai:refs/notes/ai"])
        .unwrap();

    assert!(
        upstream
            .read_authorship_note(&remote_commit.commit_sha)
            .is_some(),
        "precondition: force-pushed target note must exist on the remote"
    );
    assert!(
        local
            .read_authorship_note(&remote_commit.commit_sha)
            .is_none(),
        "precondition: local clone must not have fetched the force-pushed target note"
    );

    local.git(&["pull", "--rebase"]).unwrap();
    assert_eq!(
        local.git(&["rev-parse", "HEAD"]).unwrap().trim(),
        remote_commit.commit_sha,
        "pull should accept the force-pushed remote target without replaying a local commit"
    );
    local_file.assert_committed_lines(crate::lines![
        "base".unattributed_human(),
        "old AI line".ai(),
        "remote AI line".ai(),
    ]);
}

#[test]
fn test_rejected_push_failed_pull_then_pull_rebase_preserves_committed_ai_authorship() {
    let (local, upstream) = TestRepo::new_with_remote();

    let mut readme = local.filename("README.md");
    readme.set_contents(vec!["# Test Repo".to_string()]);
    local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");
    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.set_contents(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
    let local_ai_commit = local
        .stage_all_and_commit("add AI feature")
        .expect("AI feature commit should succeed");

    assert!(
        local
            .read_authorship_note(&local_ai_commit.commit_sha)
            .is_some(),
        "precondition: original local AI commit should have authorship note"
    );

    let branch = local.current_branch();
    let contributor_parent = tempfile::tempdir().expect("contributor temp dir");
    let contributor_path = contributor_parent.path().join("contributor");
    local
        .git_og(&[
            "clone",
            upstream
                .path()
                .to_str()
                .expect("upstream path should be utf-8"),
            contributor_path
                .to_str()
                .expect("contributor path should be utf-8"),
        ])
        .expect("clone contributor should succeed");
    let contributor =
        TestRepo::new_at_path_with_daemon_scope(&contributor_path, DaemonTestScope::NoDaemon);
    std::fs::write(
        contributor.path().join("upstream_change.txt"),
        "upstream content\n",
    )
    .expect("write upstream change");
    contributor.git_og(&["add", "."]).unwrap();
    contributor
        .git_og(&["commit", "-m", "upstream divergent commit"])
        .expect("upstream commit should succeed");
    contributor
        .git_og(&["push", "origin", &format!("HEAD:{}", branch)])
        .expect("push upstream divergence should succeed");

    assert!(
        local.git(&["push"]).is_err(),
        "push should be rejected because origin has diverged"
    );
    assert!(
        local.git(&["pull"]).is_err(),
        "plain pull should fail before an explicit reconciliation strategy"
    );

    local
        .git(&["pull", "--rebase"])
        .expect("pull --rebase should succeed");

    let rebased_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_head, local_ai_commit.commit_sha,
        "HEAD should have a new SHA after rebase"
    );
    assert!(
        local.read_authorship_note(&rebased_head).is_some(),
        "rebased local AI commit should have authorship note"
    );

    ai_file.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
}

#[test]
fn test_pull_rebase_via_git_config_preserves_committed_ai_authorship() {
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Set git config to use rebase for pull (no --rebase flag needed)
    local
        .git(&["config", "pull.rebase", "true"])
        .expect("set pull.rebase should succeed");

    // Perform plain pull (should rebase due to config)
    local.git(&["pull"]).expect("pull should succeed");

    // Verify upstream changes arrived and commit SHA changed
    assert!(
        local.read_file("upstream_change.txt").is_some(),
        "Should have upstream_change.txt after pull"
    );

    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.local_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    // Verify AI authorship survived
    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
}

#[test]
fn test_pull_rebase_via_alias_preserves_committed_ai_authorship() {
    // Regression: `git up` where `up = pull --rebase`. Git expands the alias
    // before writing the reflog (label `pull --rebase ... (start)`), but the
    // daemon previously reconstructed the pull action from the literal alias
    // token `up`, so the span matcher never matched and the rebased AI commit's
    // authorship note was dropped. The invocation must expand to `pull
    // --rebase` so attribution migrates with the rebase.
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Define an alias that expands to `pull --rebase`.
    local
        .git(&["config", "alias.up", "pull --rebase"])
        .expect("set alias.up should succeed");

    // Drive the rebase entirely through the alias (no explicit --rebase flag).
    local.git(&["up"]).expect("aliased pull should succeed");

    // Verify upstream changes arrived and the commit SHA changed (real rebase).
    assert!(
        local.read_file("upstream_change.txt").is_some(),
        "Should have upstream_change.txt after aliased pull --rebase"
    );

    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.local_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    // Verify AI authorship survived the alias-driven rebase.
    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
}

#[test]
fn test_pull_rebase_via_zero_arg_alias_and_git_config_preserves_committed_ai_authorship() {
    // Regression: `git up` where `up = pull` and `pull.rebase=true`. The alias
    // expands to `pull` with no explicit args, so the normalized invocation must
    // still keep `pull` visible instead of falling back to the raw alias token.
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    local
        .git(&["config", "alias.up", "pull"])
        .expect("set alias.up should succeed");
    local
        .git(&["config", "pull.rebase", "true"])
        .expect("set pull.rebase should succeed");

    local.git(&["up"]).expect("aliased pull should succeed");

    assert!(
        local.read_file("upstream_change.txt").is_some(),
        "Should have upstream_change.txt after aliased config-driven pull --rebase"
    );

    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.local_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    let mut ai_file = local.filename("ai_feature.txt");
    ai_file.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);
}

// =============================================================================
// Pull --rebase --autostash with uncommitted changes
// =============================================================================

#[test]
fn test_pull_rebase_autostash_preserves_uncommitted_ai_attribution() {
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Add uncommitted AI changes on top of the committed ones
    let mut uncommitted_ai = local.filename("uncommitted_ai.txt");
    uncommitted_ai.set_contents(vec![
        "AI generated line 1".ai(),
        "AI generated line 2".ai(),
        "AI generated line 3".ai(),
    ]);

    local
        .git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Pull --rebase --autostash: uncommitted changes get stashed/unstashed
    local
        .git(&["pull", "--rebase", "--autostash"])
        .expect("pull --rebase --autostash should succeed");

    // Commit the previously-uncommitted changes
    local
        .stage_all_and_commit("commit after rebase pull")
        .expect("commit should succeed");

    uncommitted_ai.assert_lines_and_blame(vec![
        "AI generated line 1".ai(),
        "AI generated line 2".ai(),
        "AI generated line 3".ai(),
    ]);
}

#[test]
fn test_pull_rebase_autostash_with_mixed_attribution() {
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Create local uncommitted changes with mixed human and AI attribution
    let mut mixed_file = local.filename("mixed_work.txt");
    mixed_file.set_contents(vec![
        "Human written line 1".human(),
        "AI generated line 1".ai(),
        "Human written line 2".human(),
        "AI generated line 2".ai(),
    ]);

    local
        .git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Pull --rebase --autostash
    local
        .git(&["pull", "--rebase", "--autostash"])
        .expect("pull --rebase --autostash should succeed");

    // Commit and verify mixed attribution is preserved
    local
        .stage_all_and_commit("commit with mixed attribution")
        .expect("commit should succeed");

    mixed_file.assert_lines_and_blame(vec![
        "Human written line 1".human(),
        "AI generated line 1".ai(),
        "Human written line 2".human(),
        "AI generated line 2".ai(),
    ]);
}

#[test]
fn test_pull_rebase_autostash_via_git_config() {
    let setup = setup_pull_test();
    let local = setup.local;

    // Set git config to always use rebase and autostash for pull
    local
        .git(&["config", "pull.rebase", "true"])
        .expect("set pull.rebase should succeed");
    local
        .git(&["config", "rebase.autoStash", "true"])
        .expect("set rebase.autoStash should succeed");

    // Create local uncommitted AI changes
    let mut ai_file = local.filename("ai_config_test.txt");
    ai_file.set_contents(vec!["AI line via config".ai()]);

    local
        .git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Perform regular pull (should use rebase+autostash from config)
    local.git(&["pull"]).expect("pull should succeed");

    // Commit and verify AI attribution is preserved
    local
        .stage_all_and_commit("commit after config-based rebase pull")
        .expect("commit should succeed");

    ai_file.assert_lines_and_blame(vec!["AI line via config".ai()]);
}

// =============================================================================
// Pull --rebase with both committed AND uncommitted changes
// =============================================================================

#[test]
fn test_pull_rebase_committed_and_autostash_preserves_all_authorship() {
    let setup = setup_divergent_pull_test();
    let local = setup.local;

    // Add uncommitted AI changes on top of the committed AI commit
    let mut uncommitted_ai = local.filename("uncommitted_ai.txt");
    uncommitted_ai.set_contents(vec!["Uncommitted AI line".ai()]);
    local
        .git_ai(&["checkpoint", "mock_ai"])
        .expect("checkpoint should succeed");

    // Pull --rebase --autostash: committed changes get rebased, uncommitted get stashed
    local
        .git(&["pull", "--rebase", "--autostash"])
        .expect("pull --rebase --autostash should succeed");

    // Commit the previously-uncommitted changes
    local
        .stage_all_and_commit("commit uncommitted AI work")
        .expect("commit should succeed");

    // Verify committed AI authorship survived the rebase
    let mut committed_ai = local.filename("ai_feature.txt");
    committed_ai.assert_lines_and_blame(vec![
        "AI generated feature line 1".ai(),
        "AI generated feature line 2".ai(),
    ]);

    // Verify uncommitted AI authorship survived the autostash cycle
    uncommitted_ai.assert_lines_and_blame(vec!["Uncommitted AI line".ai()]);
}

#[test]
fn test_pull_rebase_skip_commit_does_not_map_entire_upstream_history() {
    let (local, _upstream, local_ai_sha) = setup_pull_rebase_skip_test();

    local
        .git(&["pull", "--rebase"])
        .expect("pull --rebase should succeed");

    // HEAD should move away from original local commit onto upstream tip.
    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    assert_ne!(
        new_head, local_ai_sha,
        "HEAD should have moved to upstream history after skipped rebase"
    );

    // Local commit was duplicated upstream via equivalent patch, so rebase should skip it.
    // Verify via git notes that the upstream-only commits (upstream_extra_1, upstream_extra_2)
    // did NOT receive AI authorship notes from the skipped local commit.
    // Walk backwards from HEAD: HEAD = upstream_extra_2, HEAD~1 = upstream_extra_1
    let upstream_extra_2 = new_head.clone();
    let upstream_extra_1 = local
        .git(&["rev-parse", "HEAD~1"])
        .expect("rev-parse HEAD~1")
        .trim()
        .to_string();

    assert!(
        local.read_authorship_note(&upstream_extra_2).is_none()
            || !local
                .read_authorship_note(&upstream_extra_2)
                .unwrap()
                .contains("ai_feature.txt"),
        "upstream_extra_2 should not have AI authorship notes from the skipped local commit"
    );
    assert!(
        local.read_authorship_note(&upstream_extra_1).is_none()
            || !local
                .read_authorship_note(&upstream_extra_1)
                .unwrap()
                .contains("ai_feature.txt"),
        "upstream_extra_1 should not have AI authorship notes from the skipped local commit"
    );
}

// =============================================================================
// Pull --rebase with conflict resolution (notes lost due to SHA rewrite)
// Reproduces: docs/test-rebase-notes-los.md
// =============================================================================

/// Setup for the two-session conflict scenario:
/// Session A and Session B both start from the same commit. Session A pushes
/// AI-authored changes to a shared file. Session B independently makes
/// AI-authored changes to the same file (without pulling). When Session B
/// rebases, the shared file conflicts. After resolution, AI authorship notes
/// should be preserved on the new (rebased) commit SHAs.
struct ConflictPullTestSetup {
    local: TestRepo,
    #[allow(dead_code)]
    upstream: TestRepo,
    /// SHA of Session B's local AI commit (will get a new SHA after rebase)
    session_b_ai_commit_sha: String,
}

/// Creates a test setup that reproduces the notes-lost-on-rebase scenario:
/// 1. Creates upstream (bare) and local (clone) repos
/// 2. Makes an initial commit with a shared file, pushes to upstream
/// 3. Simulates Session A: edits the shared file with AI content, pushes
/// 4. Simulates Session B: resets local to before Session A's push,
///    edits the same shared file with different AI content (diverged)
///
/// After this setup:
/// - upstream has: initial + Session A's AI commit (edits README.md)
/// - local has: initial + Session B's AI commit (edits README.md)
/// - `git pull --rebase` will conflict on README.md
fn setup_conflict_pull_test() -> ConflictPullTestSetup {
    let (local, upstream) = TestRepo::new_with_remote();

    // Initial commit: shared file that both sessions will edit
    let mut readme = local.filename("README.md");
    readme.set_contents(vec![
        "# Project".human(),
        "Initial content line 1".human(),
        "Initial content line 2".human(),
    ]);
    let initial = local
        .stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");

    local
        .git(&["push", "-u", "origin", "HEAD"])
        .expect("push initial commit should succeed");

    let branch = local.current_branch();

    // --- Session A: edit README.md with AI content, push to upstream ---
    let mut session_a_readme = local.filename("README.md");
    session_a_readme.set_contents(vec![
        "# Project".human(),
        "Session A: AI-enhanced line 1".ai(),
        "Session A: AI-enhanced line 2".ai(),
    ]);
    local
        .stage_all_and_commit("Session A: AI enhancements")
        .expect("Session A commit should succeed");

    local
        .git(&["push", "origin", &format!("HEAD:{}", branch)])
        .expect("Session A push should succeed");

    // --- Session B: reset to initial (as if Session B never saw A's push) ---
    local
        .git(&["reset", "--hard", &initial.commit_sha])
        .expect("reset to initial should succeed");

    // Session B: edit the same file with different AI content
    let mut session_b_readme = local.filename("README.md");
    session_b_readme.set_contents(vec![
        "# Project".human(),
        "Session B: AI-generated line 1".ai(),
        "Session B: AI-generated line 2".ai(),
    ]);
    local
        .stage_all_and_commit("Session B: AI feature")
        .expect("Session B commit should succeed");

    let session_b_sha = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    ConflictPullTestSetup {
        local,
        upstream,
        session_b_ai_commit_sha: session_b_sha,
    }
}

#[test]
fn test_pull_rebase_with_conflict_preserves_ai_notes() {
    let setup = setup_conflict_pull_test();
    let local = setup.local;

    // Verify Session B's AI commit has authorship notes before rebase
    let pre_rebase_note = local.read_authorship_note(&setup.session_b_ai_commit_sha);
    assert!(
        pre_rebase_note.is_some(),
        "Session B's AI commit should have authorship notes before rebase"
    );

    // Configure pull to use rebase (matching the doc scenario)
    local
        .git(&["config", "pull.rebase", "true"])
        .expect("set pull.rebase should succeed");

    // Fetch so we know about upstream's diverged state
    local
        .git(&["fetch", "origin"])
        .expect("fetch should succeed");

    // Pull will rebase — this should conflict on README.md
    let pull_result = local.git(&["pull"]);
    assert!(
        pull_result.is_err(),
        "pull --rebase should fail due to conflict on README.md"
    );

    // Resolve the conflict: keep both sessions' contributions
    use std::fs;
    fs::write(
        local.path().join("README.md"),
        "# Project\nSession A: AI-enhanced line 1\nSession A: AI-enhanced line 2\nSession B: AI-generated line 1\nSession B: AI-generated line 2\n",
    )
    .expect("writing resolved file should succeed");

    local
        .git(&["add", "README.md"])
        .expect("staging resolved file should succeed");

    local
        .git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    // The rebased commit has a new SHA
    let new_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.session_b_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    // This is the core assertion from the doc:
    // After rebase, the new commit SHA should still have authorship notes.
    let post_rebase_note = local.read_authorship_note(&new_head);
    assert!(
        post_rebase_note.is_some(),
        "Rebased commit should have authorship notes (notes should follow SHA rewrite)"
    );

    // Verify the note content references the AI-authored file
    let note_content = post_rebase_note.unwrap();
    assert!(
        note_content.contains("README.md"),
        "Authorship note should reference README.md, got: {}",
        note_content
    );
}

// =============================================================================
// Pull --rebase abort preserves original notes
// =============================================================================

#[test]
fn test_pull_rebase_with_conflict_abort_preserves_original_notes() {
    let setup = setup_conflict_pull_test();
    let local = setup.local;

    // Verify Session B's AI commit has authorship notes before rebase
    let pre_rebase_note = local.read_authorship_note(&setup.session_b_ai_commit_sha);
    assert!(
        pre_rebase_note.is_some(),
        "Session B's AI commit should have authorship notes before rebase"
    );

    // Configure pull to use rebase
    local
        .git(&["config", "pull.rebase", "true"])
        .expect("set pull.rebase should succeed");

    // Fetch so we know about upstream's diverged state
    local
        .git(&["fetch", "origin"])
        .expect("fetch should succeed");

    // Pull will rebase — this should conflict on README.md
    let pull_result = local.git(&["pull"]);
    assert!(
        pull_result.is_err(),
        "pull --rebase should fail due to conflict on README.md"
    );

    // Abort the rebase instead of resolving
    local
        .git(&["rebase", "--abort"])
        .expect("rebase --abort should succeed");

    // Verify HEAD is back to Session B's original SHA
    let current_head = local
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_eq!(
        current_head, setup.session_b_ai_commit_sha,
        "HEAD should be back to Session B's original commit after abort"
    );

    // Verify authorship notes on the original SHA are still intact
    let post_abort_note = local.read_authorship_note(&setup.session_b_ai_commit_sha);
    assert!(
        post_abort_note.is_some(),
        "Session B's AI commit should still have authorship notes after abort"
    );
}

// =============================================================================
// Regular (non-pull) rebase with conflict scenarios
// =============================================================================

/// Setup for regular rebase conflict tests: local-only repo with a feature branch
/// that has AI commits conflicting with main.
struct RegularRebaseConflictSetup {
    repo: TestRepo,
    /// SHA of the AI commit on the feature branch
    feature_ai_commit_sha: String,
    /// SHA of the conflicting commit on the default branch
    main_conflict_commit_sha: String,
    /// Name of the default branch
    default_branch: String,
}

fn setup_regular_rebase_conflict() -> RegularRebaseConflictSetup {
    let repo = TestRepo::new();

    // Create initial commit with a shared file
    let mut shared_file = repo.filename("shared.txt");
    shared_file.set_contents(vec!["line 1".human(), "line 2".human()]);
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");

    let default_branch = repo.current_branch();

    // Create feature branch with AI-authored changes to the shared file
    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout -b feature should succeed");

    let mut feature_file = repo.filename("shared.txt");
    feature_file.set_contents(vec!["line 1".human(), "AI feature line 2".ai()]);
    repo.stage_all_and_commit("AI feature changes")
        .expect("AI feature commit should succeed");

    let feature_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    // Make conflicting change on main
    repo.git(&["checkout", &default_branch])
        .expect("checkout main should succeed");

    let mut main_file = repo.filename("shared.txt");
    main_file.set_contents(vec!["line 1".human(), "main change line 2".human()]);
    repo.stage_all_and_commit("main conflicting change")
        .expect("main commit should succeed");
    let main_conflict_commit_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    // Switch back to feature
    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");

    RegularRebaseConflictSetup {
        repo,
        feature_ai_commit_sha: feature_sha,
        main_conflict_commit_sha,
        default_branch,
    }
}

fn setup_regular_rebase_conflict_with_trailing_newlines() -> RegularRebaseConflictSetup {
    use std::fs;

    let repo = TestRepo::new();
    let shared_path = repo.path().join("shared.txt");

    fs::write(&shared_path, "line 1\nline 2\n").expect("write initial file");
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .expect("initial known-human checkpoint should succeed");
    repo.stage_all_and_commit("initial commit")
        .expect("initial commit should succeed");

    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "feature"])
        .expect("checkout -b feature should succeed");

    fs::write(&shared_path, "line 1\nAI feature line 2\n").expect("write feature file");
    repo.git_ai(&["checkpoint", "mock_ai", "shared.txt"])
        .expect("feature AI checkpoint should succeed");
    repo.stage_all_and_commit("AI feature changes")
        .expect("AI feature commit should succeed");

    let feature_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    repo.git(&["checkout", &default_branch])
        .expect("checkout main should succeed");

    fs::write(&shared_path, "line 1\nmain change line 2\n").expect("write main file");
    repo.git_ai(&["checkpoint", "mock_known_human", "shared.txt"])
        .expect("main known-human checkpoint should succeed");
    repo.stage_all_and_commit("main conflicting change")
        .expect("main commit should succeed");
    let main_conflict_commit_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    repo.git(&["checkout", "feature"])
        .expect("checkout feature should succeed");

    RegularRebaseConflictSetup {
        repo,
        feature_ai_commit_sha: feature_sha,
        main_conflict_commit_sha,
        default_branch,
    }
}

fn session_keys(log: &AuthorshipLog) -> BTreeSet<String> {
    log.metadata.sessions.keys().cloned().collect()
}

fn checkpoint_claude_file_edit(
    repo: &TestRepo,
    event_name: &str,
    file_path: &str,
    tool_use_id: &str,
) {
    let transcript_path = repo.path().join(".git-ai-test-claude-session.jsonl");
    std::fs::write(
        &transcript_path,
        "{\"type\":\"message\",\"message\":{\"model\":\"claude-sonnet-4-5\"}}\n",
    )
    .expect("write claude transcript fixture");
    let absolute_file_path = repo.path().join(file_path);
    let hook_input = json!({
        "cwd": repo.path(),
        "transcript_path": transcript_path,
        "hook_event_name": event_name,
        "tool_name": "Edit",
        "session_id": "test-claude-rebase-conflict-session",
        "tool_use_id": tool_use_id,
        "tool_input": {
            "file_path": absolute_file_path,
        },
    })
    .to_string();

    repo.git_ai(&["checkpoint", "claude", "--hook-input", &hook_input])
        .expect("claude checkpoint should succeed");
}

fn attestation_author_keys(log: &AuthorshipLog, path: &str) -> BTreeSet<String> {
    log.attestations
        .iter()
        .filter(|attestation| attestation.file_path == path)
        .flat_map(|attestation| attestation.entries.iter())
        .map(|entry| {
            entry
                .hash
                .split("::")
                .next()
                .unwrap_or(&entry.hash)
                .to_string()
        })
        .collect()
}

#[test]
fn test_regular_rebase_with_conflict_preserves_ai_notes() {
    let setup = setup_regular_rebase_conflict();
    let repo = setup.repo;

    // Verify AI commit has authorship notes before rebase
    let pre_rebase_note = repo.read_authorship_note(&setup.feature_ai_commit_sha);
    assert!(
        pre_rebase_note.is_some(),
        "Feature AI commit should have authorship notes before rebase"
    );
    let pre_rebase_note = pre_rebase_note.unwrap();
    let pre_rebase_log =
        AuthorshipLog::deserialize_from_string(&pre_rebase_note).expect("parse pre-rebase note");
    assert!(
        !pre_rebase_log.metadata.sessions.is_empty(),
        "precondition: feature AI commit should have session metadata"
    );

    // Rebase feature onto main — should conflict on shared.txt
    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    // Resolve the conflict: keep both changes
    use std::fs;
    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI feature line 2\n",
    )
    .expect("writing resolved file should succeed");

    repo.git(&["add", "shared.txt"])
        .expect("staging resolved file should succeed");

    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    // The rebased commit has a new SHA
    let new_head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_ne!(
        new_head, setup.feature_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    // Verify authorship notes were preserved on the new commit
    let post_rebase_note = repo.read_authorship_note(&new_head);
    assert!(
        post_rebase_note.is_some(),
        "Rebased commit should have authorship notes (notes should follow SHA rewrite)"
    );

    // After conflict resolution, AI-attributed lines fall inside diff hunks
    // (git diff-tree shows the region as modified), so attribution is correctly dropped.
    // The note exists (metadata preserved) but shared.txt has no attributed lines.
    let note_content = post_rebase_note.unwrap();
    let post_rebase_log =
        AuthorshipLog::deserialize_from_string(&note_content).expect("parse post-rebase note");
    assert_eq!(
        post_rebase_log.metadata.sessions, pre_rebase_log.metadata.sessions,
        "session metadata should be preserved even when changed-hunk attestations are dropped"
    );
    assert!(
        !note_content.contains("shared.txt"),
        "Authorship note should NOT reference shared.txt (lines inside diff hunk), got: {}",
        note_content
    );
}

#[test]
fn test_regular_rebase_two_conflicts_ai_rewrite_after_skipped_conflict_is_attributed() {
    use std::fs;

    let repo = TestRepo::new();
    let jokes_path = repo.path().join("jokes-programming.csv");
    let base = "\
setup,punchline
How many programmers does it take to change a light bulb?,None that's a hardware problem
Why do Java developers wear glasses?,Because they don't C#
Why did the programmer quit his job?,Because he didn't get arrays
";

    fs::write(&jokes_path, base).expect("write base jokes");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("base AI checkpoint should succeed");
    repo.stage_all_and_commit("Base jokes")
        .expect("base commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "scenario-2-multi-conflict-same-file"])
        .expect("checkout feature branch should succeed");
    fs::write(
        &jokes_path,
        format!(
            "{}Why do Python developers make bad partners?,They only speak one language\n",
            base
        ),
    )
    .expect("write first feature joke");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("first feature AI checkpoint should succeed");
    repo.stage_all_and_commit("Add Python joke")
        .expect("first feature commit should succeed");

    fs::write(
        &jokes_path,
        format!(
            "{}Why do Python developers make bad partners?,They only speak one language\nHow many Rust developers does it take to change a lightbulb?,Two one to change it and one to write a song about the old one\n",
            base
        ),
    )
    .expect("write second feature joke");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("second feature AI checkpoint should succeed");
    repo.stage_all_and_commit("Add Rust joke")
        .expect("second feature commit should succeed");

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    let main = format!(
        "{}Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25\nWhy did the developer go broke?,Because he used up all his cache\n",
        base
    );
    fs::write(&jokes_path, &main).expect("write main jokes");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("main AI checkpoint should succeed");
    repo.stage_all_and_commit("Add C++ jokes")
        .expect("main commit should succeed");

    let rebase_result = repo.git(&[
        "rebase",
        &default_branch,
        "scenario-2-multi-conflict-same-file",
    ]);
    assert!(
        rebase_result.is_err(),
        "first rebase stop should conflict on the Python joke"
    );

    checkpoint_claude_file_edit(
        &repo,
        "PreToolUse",
        "jokes-programming.csv",
        "resolve-first",
    );
    fs::write(&jokes_path, &main).expect("resolve first conflict by keeping main side");
    checkpoint_claude_file_edit(
        &repo,
        "PostToolUse",
        "jokes-programming.csv",
        "resolve-first",
    );
    repo.git(&["add", "jokes-programming.csv"])
        .expect("stage first conflict resolution");
    let second_stop = repo.git(&["rebase", "--skip"]);
    assert!(
        second_stop.is_err(),
        "skipping the first feature commit should immediately stop on the Rust conflict"
    );

    let rewritten = format!(
        "{}Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25\nWhy did the developer go broke?,Because he used up all his cache\nWhy do Rust developers write songs?,Because they're afraid of memory leaks in the lyrics\n",
        base
    );
    repo.git_ai(&["checkpoint", "human", "jokes-programming.csv"])
        .expect("pre-resolution checkpoint should succeed");
    fs::write(&jokes_path, rewritten).expect("rewrite second conflict resolution");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("AI resolution checkpoint should succeed");
    repo.git(&["add", "jokes-programming.csv"])
        .expect("stage AI conflict resolution");
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should finish");

    let mut jokes = repo.filename("jokes-programming.csv");
    jokes.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "How many programmers does it take to change a light bulb?,None that's a hardware problem"
            .ai(),
        "Why do Java developers wear glasses?,Because they don't C#".ai(),
        "Why did the programmer quit his job?,Because he didn't get arrays".ai(),
        "Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25"
            .ai(),
        "Why did the developer go broke?,Because he used up all his cache".ai(),
        "Why do Rust developers write songs?,Because they're afraid of memory leaks in the lyrics"
            .ai(),
    ]);
}

#[test]
fn test_regular_rebase_conflict_ai_resolution_preserves_original_and_resolution_sessions() {
    use std::fs;

    let setup = setup_regular_rebase_conflict();
    let repo = setup.repo;
    let original_note = repo
        .read_authorship_note(&setup.feature_ai_commit_sha)
        .expect("feature AI commit should have authorship note");
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");
    let original_sessions = session_keys(&original_log);
    assert!(
        !original_sessions.is_empty(),
        "precondition: original feature note should contain session metadata"
    );

    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    repo.git_ai(&["checkpoint", "human", "shared.txt"])
        .expect("pre-resolution checkpoint should succeed");
    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI resolved line 2",
    )
    .expect("write AI conflict resolution");
    repo.git_ai(&["checkpoint", "mock_ai", "shared.txt"])
        .expect("AI resolution checkpoint should succeed");

    repo.git(&["add", "shared.txt"])
        .expect("staging resolved file should succeed");
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    let rebased_head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    assert_ne!(
        rebased_head, setup.feature_ai_commit_sha,
        "HEAD should have a new SHA after rebase"
    );

    let rebased_note = repo
        .read_authorship_note(&rebased_head)
        .expect("rebased commit should have authorship note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    let rebased_sessions = session_keys(&rebased_log);
    let resolution_sessions = rebased_sessions
        .difference(&original_sessions)
        .cloned()
        .collect::<BTreeSet<_>>();

    assert!(
        original_sessions.is_subset(&rebased_sessions),
        "rebased note should preserve original feature session metadata; original={:?}, rebased={:?}; note={}",
        original_sessions,
        rebased_sessions,
        rebased_note
    );
    assert!(
        !resolution_sessions.is_empty(),
        "rebased note should contain a new AI conflict-resolution session; original={:?}, rebased={:?}",
        original_sessions,
        rebased_sessions
    );

    let shared_authors = attestation_author_keys(&rebased_log, "shared.txt");
    assert!(
        !shared_authors.is_empty(),
        "AI resolution should create shared.txt attribution"
    );
    assert!(
        shared_authors
            .iter()
            .any(|author| resolution_sessions.contains(author)),
        "shared.txt attribution should belong to resolution session; authors={:?}, resolution_sessions={:?}",
        shared_authors,
        resolution_sessions
    );
    assert!(
        shared_authors.is_disjoint(&original_sessions),
        "original conflict-hunk attribution should be dropped, not carried as file attribution; authors={:?}, original_sessions={:?}",
        shared_authors,
        original_sessions
    );

    let mut final_file = repo.filename("shared.txt");
    final_file.assert_committed_lines(crate::lines![
        "line 1".human(),
        "main change line 2".human(),
        "AI resolved line 2".ai(),
    ]);
}

#[test]
fn test_regular_rebase_conflict_keep_feature_side_preserves_feature_attribution() {
    use std::fs;

    let setup = setup_regular_rebase_conflict();
    let repo = setup.repo;
    let original_note = repo
        .read_authorship_note(&setup.feature_ai_commit_sha)
        .expect("feature AI commit should have authorship note");
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");
    let original_sessions = session_keys(&original_log);
    assert!(
        !original_sessions.is_empty(),
        "precondition: original feature note should contain session metadata"
    );

    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    fs::write(repo.path().join("shared.txt"), "line 1\nAI feature line 2")
        .expect("write feature-side conflict resolution");
    repo.git(&["add", "shared.txt"])
        .expect("staging resolved file should succeed");
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    let rebased_head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_head)
        .expect("rebased commit should have authorship note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    let shared_authors = attestation_author_keys(&rebased_log, "shared.txt");
    assert!(
        shared_authors
            .iter()
            .any(|author| original_sessions.contains(author)),
        "feature-side resolution should preserve feature attribution; authors={:?}, original_sessions={:?}; note={}",
        shared_authors,
        original_sessions,
        rebased_note
    );

    let mut final_file = repo.filename("shared.txt");
    final_file.assert_committed_lines(crate::lines!["line 1".human(), "AI feature line 2".ai(),]);
}

#[test]
fn test_regular_rebase_conflict_keep_both_sides_preserves_each_original_source() {
    use std::fs;

    let setup = setup_regular_rebase_conflict_with_trailing_newlines();
    let repo = setup.repo;
    let original_note = repo
        .read_authorship_note(&setup.feature_ai_commit_sha)
        .expect("feature AI commit should have authorship note");
    let original_log =
        AuthorshipLog::deserialize_from_string(&original_note).expect("parse original note");
    let original_sessions = session_keys(&original_log);
    assert!(
        !original_sessions.is_empty(),
        "precondition: original feature note should contain session metadata"
    );

    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    fs::write(
        repo.path().join("shared.txt"),
        "line 1\nmain change line 2\nAI feature line 2\n",
    )
    .expect("write keep-both conflict resolution");
    repo.git(&["add", "shared.txt"])
        .expect("staging resolved file should succeed");
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should succeed");

    let rebased_head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    let rebased_note = repo
        .read_authorship_note(&rebased_head)
        .expect("rebased commit should have authorship note");
    let rebased_log =
        AuthorshipLog::deserialize_from_string(&rebased_note).expect("parse rebased note");
    let shared_authors = attestation_author_keys(&rebased_log, "shared.txt");
    assert!(
        shared_authors
            .iter()
            .any(|author| original_sessions.contains(author)),
        "keep-both resolution should preserve feature-side attribution; authors={:?}, original_sessions={:?}; note={}",
        shared_authors,
        original_sessions,
        rebased_note
    );

    let blame = repo
        .git(&["blame", "--line-porcelain", "-L", "2,2", "--", "shared.txt"])
        .expect("git blame should succeed");
    let blamed_commit = blame
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .expect("blame should include commit sha");
    assert_eq!(
        blamed_commit, setup.main_conflict_commit_sha,
        "main-side kept line should blame to the original main conflict commit"
    );

    let mut final_file = repo.filename("shared.txt");
    final_file.assert_committed_lines(crate::lines![
        "line 1".human(),
        "main change line 2".human(),
        "AI feature line 2".ai(),
    ]);
}

#[test]
fn test_regular_rebase_conflict_keep_main_side_preserves_main_attribution() {
    use std::fs;

    let setup = setup_regular_rebase_conflict();
    let repo = setup.repo;

    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    fs::write(repo.path().join("shared.txt"), "line 1\nmain change line 2")
        .expect("write main-side conflict resolution");
    repo.git(&["add", "shared.txt"])
        .expect("staging resolved file should succeed");
    repo.git(&["rebase", "--skip"])
        .expect("main-side resolution makes the feature commit empty, so rebase should skip it");

    let head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();
    assert_eq!(
        head, setup.main_conflict_commit_sha,
        "keeping the main side should leave feature at the original main conflict commit"
    );

    let blame = repo
        .git(&["blame", "--line-porcelain", "-L", "2,2", "--", "shared.txt"])
        .expect("git blame should succeed");
    let blamed_commit = blame
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().next())
        .expect("blame should include commit sha");
    assert_eq!(
        blamed_commit, setup.main_conflict_commit_sha,
        "main-side line should blame to the original main conflict commit"
    );

    let mut final_file = repo.filename("shared.txt");
    final_file.assert_committed_lines(crate::lines![
        "line 1".human(),
        "main change line 2".human(),
    ]);
}

#[test]
fn test_regular_rebase_two_conflicts_ai_rewrite_after_empty_continue_is_attributed() {
    use std::fs;

    let repo = TestRepo::new();
    let jokes_path = repo.path().join("jokes-programming.csv");
    let base = "\
setup,punchline
How many programmers does it take to change a light bulb?,None that's a hardware problem
Why do Java developers wear glasses?,Because they don't C#
Why did the programmer quit his job?,Because he didn't get arrays
";

    fs::write(&jokes_path, base).expect("write base jokes");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("base AI checkpoint should succeed");
    repo.stage_all_and_commit("Base jokes")
        .expect("base commit should succeed");
    let default_branch = repo.current_branch();

    repo.git(&["checkout", "-b", "scenario-2-multi-conflict-same-file"])
        .expect("checkout feature branch should succeed");
    fs::write(
        &jokes_path,
        format!(
            "{}Why do Python developers make bad partners?,They only speak one language\n",
            base
        ),
    )
    .expect("write first feature joke");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("first feature AI checkpoint should succeed");
    repo.stage_all_and_commit("Add Python joke")
        .expect("first feature commit should succeed");

    fs::write(
        &jokes_path,
        format!(
            "{}Why do Python developers make bad partners?,They only speak one language\nHow many Rust developers does it take to change a lightbulb?,Two one to change it and one to write a song about the old one\n",
            base
        ),
    )
    .expect("write second feature joke");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("second feature AI checkpoint should succeed");
    repo.stage_all_and_commit("Add Rust joke")
        .expect("second feature commit should succeed");

    repo.git(&["checkout", &default_branch])
        .expect("checkout default branch should succeed");
    let main = format!(
        "{}Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25\nWhy did the developer go broke?,Because he used up all his cache\n",
        base
    );
    fs::write(&jokes_path, &main).expect("write main jokes");
    repo.git_ai(&["checkpoint", "mock_ai", "jokes-programming.csv"])
        .expect("main AI checkpoint should succeed");
    repo.stage_all_and_commit("Add C++ jokes")
        .expect("main commit should succeed");

    let rebase_result = repo.git(&[
        "rebase",
        &default_branch,
        "scenario-2-multi-conflict-same-file",
    ]);
    assert!(
        rebase_result.is_err(),
        "first rebase stop should conflict on the Python joke"
    );

    fs::write(&jokes_path, &main).expect("resolve first conflict by keeping main side");
    repo.git(&["add", "jokes-programming.csv"])
        .expect("stage first conflict resolution");
    let second_stop = repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None);
    assert!(
        second_stop.is_err(),
        "continuing the empty first resolution should stop on the Rust conflict"
    );

    let rewritten = format!(
        "{}Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25\nWhy did the developer go broke?,Because he used up all his cache\nWhat's a programmer's favorite hangout place?,Foo Bar\n",
        base
    );
    checkpoint_claude_file_edit(
        &repo,
        "PreToolUse",
        "jokes-programming.csv",
        "resolve-second",
    );
    fs::write(&jokes_path, rewritten).expect("rewrite second conflict resolution");
    checkpoint_claude_file_edit(
        &repo,
        "PostToolUse",
        "jokes-programming.csv",
        "resolve-second",
    );
    repo.git(&["add", "jokes-programming.csv"])
        .expect("stage AI conflict resolution");
    repo.git_with_env(&["rebase", "--continue"], &[("GIT_EDITOR", "true")], None)
        .expect("rebase --continue should finish");

    let mut jokes = repo.filename("jokes-programming.csv");
    jokes.assert_committed_lines(crate::lines![
        "setup,punchline".ai(),
        "How many programmers does it take to change a light bulb?,None that's a hardware problem"
            .ai(),
        "Why do Java developers wear glasses?,Because they don't C#".ai(),
        "Why did the programmer quit his job?,Because he didn't get arrays".ai(),
        "Why do C++ developers get halloween mixed up with christmas?,Because Oct31 equals Dec25"
            .ai(),
        "Why did the developer go broke?,Because he used up all his cache".ai(),
        "What's a programmer's favorite hangout place?,Foo Bar".ai(),
    ]);
}

#[test]
fn test_regular_rebase_with_conflict_abort_preserves_original_notes() {
    let setup = setup_regular_rebase_conflict();
    let repo = setup.repo;

    // Verify AI commit has authorship notes before rebase
    let pre_rebase_note = repo.read_authorship_note(&setup.feature_ai_commit_sha);
    assert!(
        pre_rebase_note.is_some(),
        "Feature AI commit should have authorship notes before rebase"
    );

    // Rebase feature onto main — should conflict on shared.txt
    let rebase_result = repo.git(&["rebase", &setup.default_branch]);
    assert!(
        rebase_result.is_err(),
        "rebase should fail due to conflict on shared.txt"
    );

    // Abort the rebase
    repo.git(&["rebase", "--abort"])
        .expect("rebase --abort should succeed");

    // Verify HEAD is back to original feature SHA
    let current_head = repo
        .git(&["rev-parse", "HEAD"])
        .expect("rev-parse should succeed")
        .trim()
        .to_string();

    assert_eq!(
        current_head, setup.feature_ai_commit_sha,
        "HEAD should be back to original feature commit after abort"
    );

    // Verify authorship notes on the original SHA are still intact
    let post_abort_note = repo.read_authorship_note(&setup.feature_ai_commit_sha);
    assert!(
        post_abort_note.is_some(),
        "Feature AI commit should still have authorship notes after abort"
    );
}

crate::reuse_tests_in_worktree!(
    test_fast_forward_pull_preserves_ai_attribution,
    test_fast_forward_pull_without_local_changes,
    test_pull_rebase_preserves_committed_ai_authorship,
    test_pull_rebase_force_pushed_target_preserves_remote_authorship_note,
    test_pull_rebase_via_git_config_preserves_committed_ai_authorship,
    test_pull_rebase_via_zero_arg_alias_and_git_config_preserves_committed_ai_authorship,
    test_pull_rebase_autostash_preserves_uncommitted_ai_attribution,
    test_pull_rebase_autostash_with_mixed_attribution,
    test_pull_rebase_autostash_via_git_config,
    test_pull_rebase_committed_and_autostash_preserves_all_authorship,
    test_pull_rebase_skip_commit_does_not_map_entire_upstream_history,
    test_pull_rebase_with_conflict_preserves_ai_notes,
    test_pull_rebase_with_conflict_abort_preserves_original_notes,
    test_regular_rebase_with_conflict_preserves_ai_notes,
    test_regular_rebase_two_conflicts_ai_rewrite_after_skipped_conflict_is_attributed,
    test_regular_rebase_two_conflicts_ai_rewrite_after_empty_continue_is_attributed,
    test_regular_rebase_conflict_ai_resolution_preserves_original_and_resolution_sessions,
    test_regular_rebase_conflict_keep_feature_side_preserves_feature_attribution,
    test_regular_rebase_conflict_keep_both_sides_preserves_each_original_source,
    test_regular_rebase_conflict_keep_main_side_preserves_main_attribution,
    test_regular_rebase_with_conflict_abort_preserves_original_notes,
);
