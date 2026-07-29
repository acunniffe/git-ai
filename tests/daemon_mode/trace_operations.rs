use super::*;

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_two_commits_preserve_ai_lines() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-two-ai-lines.txt";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(&file_path, "base\n").expect("failed to seed base file");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for first append");
        writeln!(f, "test").expect("failed to append first ai line");
    }
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("first delegated ai checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first ai line should succeed");

    {
        let mut f = fs::OpenOptions::new()
            .append(true)
            .open(&file_path)
            .expect("failed to open file for second append");
        writeln!(f, "test1").expect("failed to append second ai line");
    }
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("second delegated ai checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "first ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging second ai line should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "second ai line"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_lines_and_blame(lines!["base", "test".ai(), "test1".ai()]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkpoint_stage_checkpoint_non_adjacent_hunks_survive_split_commits() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let file_rel = "daemon-non-adjacent.md";
    let file_path = repo.path().join(file_rel);
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    let initial = "\
Top line

**Section Alpha**
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, initial).expect("failed to write initial content");
    traced_git_with_env(
        &repo,
        &["add", file_rel],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    let first_ai_hunk = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

**Section Omega**
omega body
";
    fs::write(&file_path, first_ai_hunk).expect("failed to write first hunk content");
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("first delegated checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging first hunk should succeed");

    let both_hunks = "\
Top line

### Section Alpha
alpha body

middle line 1
middle line 2

### Section Omega
omega body
";
    fs::write(&file_path, both_hunks).expect("failed to write both hunks content");
    repo.git_ai_with_env(
        &["checkpoint", "mock_ai", file_rel],
        &[("GIT_AI_DAEMON_CHECKPOINT_DELEGATE", "true")],
    )
    .expect("second delegated checkpoint should succeed");
    expected_top_level_completions += 1;
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit first staged hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("first split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    traced_git_with_env(
        &repo,
        &["add", "."],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("staging remaining hunk should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "commit second hunk"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second split commit should succeed");
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );

    let mut file = repo.filename(file_rel);
    file.assert_lines_and_blame(lines![
        "Top line",
        "".human(),
        "### Section Alpha".ai(),
        "alpha body",
        "".human(),
        "middle line 1",
        "middle line 2",
        "".human(),
        "### Section Omega".ai(),
        "omega body",
    ]);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_write_mode_applies_amend_rewrite() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("pure-trace.txt"), "line 1\n").expect("failed to write file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "initial"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit should succeed");

    fs::write(repo.path().join("pure-trace.txt"), "line 1\nline 2\n")
        .expect("failed to update file");
    traced_git_with_env(
        &repo,
        &["add", "pure-trace.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before amend should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "--amend", "-m", "initial amended"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("amend should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("rebase-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "feature\n")
        .expect("failed to write feature branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("rebase-conflict.txt"), "main\n")
        .expect("failed to write default branch change");
    traced_git_with_env(
        &repo,
        &["add", "rebase-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "feature"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    let rebase_conflict = traced_git_with_env(
        &repo,
        &["rebase", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        rebase_conflict.is_err(),
        "rebase should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["rebase", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_abort_emits_abort_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("cherry-conflict.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "topic\n")
        .expect("failed to write topic branch change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");
    fs::write(repo.path().join("cherry-conflict.txt"), "main\n")
        .expect("failed to write default branch conflicting change");
    traced_git_with_env(
        &repo,
        &["add", "cherry-conflict.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("default branch commit should succeed");

    let cherry_pick_conflict = traced_git_with_env(
        &repo,
        &["cherry-pick", topic_sha.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    );
    assert!(
        cherry_pick_conflict.is_err(),
        "cherry-pick should conflict for abort flow coverage"
    );
    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
    traced_git_with_env(
        &repo,
        &["cherry-pick", "--abort"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick abort should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_stash_main_ops_emit_stash_events() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    fs::write(repo.path().join("stash-case.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    fs::write(repo.path().join("stash-case.txt"), "base\nchange one\n")
        .expect("failed to write stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save one"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash push should succeed");
    // `git stash list` is readonly — the daemon's readonly fast-path drops it
    // before it reaches the ingest queue, so we run it without incrementing
    // expected_top_level_completions and do not expect it in the rewrite log.
    repo.git_og_with_env(&["stash", "list"], &env_refs)
        .expect("stash list should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "apply", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash apply should succeed");

    traced_git_with_env(
        &repo,
        &["reset", "--hard", "HEAD"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("reset hard should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "pop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash pop should succeed");

    traced_git_with_env(
        &repo,
        &["add", "stash-case.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add before commit should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "stash pop result"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("commit after stash pop should succeed");

    fs::write(repo.path().join("stash-case.txt"), "base\nchange two\n")
        .expect("failed to write second stash content");
    traced_git_with_env(
        &repo,
        &["stash", "push", "-m", "save two"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("second stash push should succeed");
    traced_git_with_env(
        &repo,
        &["stash", "drop", "stash@{0}"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("stash drop should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_continue_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = vec![
        (env[0].0, env[0].1.as_str()),
        (env[1].0, env[1].1.as_str()),
        ("GIT_EDITOR", "true"),
    ];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("cherry-continue.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "topic"], &env_refs)
        .expect("topic checkout should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "topic\n")
        .expect("failed to write topic change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("topic add should succeed");
    repo.git_og_with_env(&["commit", "-m", "topic change"], &env_refs)
        .expect("topic commit should succeed");
    let topic_sha = repo
        .git(&["rev-parse", "topic"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    fs::write(repo.path().join("cherry-continue.txt"), "main\n")
        .expect("failed to write main conflict change");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("main add should succeed");
    repo.git_og_with_env(&["commit", "-m", "main change"], &env_refs)
        .expect("main commit should succeed");

    let cherry_conflict = repo.git_og_with_env(&["cherry-pick", topic_sha.as_str()], &env_refs);
    assert!(
        cherry_conflict.is_err(),
        "cherry-pick should conflict before continue"
    );
    wait_for_expected_top_level_completions(&repo, 0, 9);

    fs::write(repo.path().join("cherry-continue.txt"), "resolved\n")
        .expect("failed to write resolved cherry content");
    repo.git_og_with_env(&["add", "cherry-continue.txt"], &env_refs)
        .expect("add resolved cherry content should succeed");
    repo.git_og_with_env(&["cherry-pick", "--continue"], &env_refs)
        .expect("cherry-pick continue should succeed");

    wait_for_expected_top_level_completions(&repo, 0, 11);
}

#[test]
#[serial]
fn daemon_pure_trace_socket_rebase_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit on default branch
    fs::write(repo.path().join("rebase-short.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "rebase-short.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create feature branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature branch checkout should succeed");
    fs::write(repo.path().join("feature-only.txt"), "feature content\n")
        .expect("failed to write feature file");
    traced_git_with_env(
        &repo,
        &["add", "feature-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "feature change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("feature commit should succeed");

    // Go back to default branch and add a non-conflicting commit
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default should succeed");
    fs::write(repo.path().join("main-only.txt"), "main content\n")
        .expect("failed to write main file");
    traced_git_with_env(
        &repo,
        &["add", "main-only.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "main advance"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("main commit should succeed");

    // Get the short SHA of the latest main commit
    let main_full_sha = repo
        .git(&["rev-parse", "HEAD"])
        .expect("HEAD rev-parse should succeed")
        .trim()
        .to_string();
    let main_short_sha = &main_full_sha[..7];

    // Switch to feature branch and rebase onto main using SHORT SHA
    traced_git_with_env(
        &repo,
        &["checkout", "feature-rebase-short"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout feature should succeed");
    traced_git_with_env(
        &repo,
        &["rebase", main_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("rebase with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_cherry_pick_with_short_sha_emits_complete_event() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();
    let completion_baseline = repo.daemon_total_completion_count();
    let mut expected_top_level_completions = 0u64;

    // Create base commit
    fs::write(repo.path().join("short-sha-test.txt"), "base\n").expect("failed to write base");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "base"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("base commit should succeed");

    // Create topic branch with a commit
    traced_git_with_env(
        &repo,
        &["checkout", "-b", "topic-short-sha"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic branch checkout should succeed");
    fs::write(repo.path().join("short-sha-test.txt"), "topic content\n")
        .expect("failed to write topic change");
    traced_git_with_env(
        &repo,
        &["add", "short-sha-test.txt"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic add should succeed");
    traced_git_with_env(
        &repo,
        &["commit", "-m", "topic change"],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("topic commit should succeed");

    // Get the full SHA and derive a short (7-char) prefix
    let topic_full_sha = repo
        .git(&["rev-parse", "topic-short-sha"])
        .expect("topic rev-parse should succeed")
        .trim()
        .to_string();
    let topic_short_sha = &topic_full_sha[..7];

    // Switch back to default branch
    traced_git_with_env(
        &repo,
        &["checkout", default_branch.as_str()],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("checkout default branch should succeed");

    // Cherry-pick using the SHORT SHA -- this is the key part of the test
    traced_git_with_env(
        &repo,
        &["cherry-pick", topic_short_sha],
        &env_refs,
        &mut expected_top_level_completions,
    )
    .expect("cherry-pick with short SHA should succeed");

    wait_for_expected_top_level_completions(
        &repo,
        completion_baseline,
        expected_top_level_completions,
    );
}

#[test]
#[serial]
fn daemon_pure_trace_socket_switch_tracks_success_and_conflict_failure() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("switch-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["switch", "-c", "feature"], &env_refs)
        .expect("switch -c feature should succeed");
    fs::write(repo.path().join("switch-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "switch-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");
    repo.git_og_with_env(&["switch", "feature"], &env_refs)
        .expect("switch to feature should succeed");
    repo.git_og_with_env(&["switch", default_branch.as_str()], &env_refs)
        .expect("switch back to default branch should succeed");

    fs::write(repo.path().join("switch-case.txt"), "dirty local change\n")
        .expect("failed to write dirty local change");
    let switch_failure = repo.git_og_with_env(&["switch", "feature"], &env_refs);
    assert!(
        switch_failure.is_err(),
        "switch should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 9);

    let switch_entries = completion_entries_for_command(&repo, "switch");
    let saw_switch_success = switch_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_switch_failure = switch_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_switch_success, "switch success should be tracked");
    assert!(saw_switch_failure, "switch failure should be tracked");
}

#[test]
#[serial]
fn daemon_pure_trace_socket_checkout_tracks_success_failure_and_new_branch() {
    let repo = TestRepo::new_with_daemon_scope(DaemonTestScope::NoDaemon);
    let _daemon = DaemonGuard::start(&repo);
    let trace_socket = daemon_trace_socket_path(&repo);
    let env = git_trace_env(&trace_socket);
    let env_refs = [(env[0].0, env[0].1.as_str()), (env[1].0, env[1].1.as_str())];
    let default_branch = repo.current_branch();

    fs::write(repo.path().join("checkout-case.txt"), "base\n").expect("failed to write base");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("add should succeed");
    repo.git_og_with_env(&["commit", "-m", "base"], &env_refs)
        .expect("base commit should succeed");

    repo.git_og_with_env(&["checkout", "-b", "feature"], &env_refs)
        .expect("checkout -b feature should succeed");
    fs::write(repo.path().join("checkout-case.txt"), "feature branch\n")
        .expect("failed to write feature content");
    repo.git_og_with_env(&["add", "checkout-case.txt"], &env_refs)
        .expect("feature add should succeed");
    repo.git_og_with_env(&["commit", "-m", "feature"], &env_refs)
        .expect("feature commit should succeed");

    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout default should succeed");
    repo.git_og_with_env(&["checkout", "feature"], &env_refs)
        .expect("checkout feature should succeed");
    repo.git_og_with_env(&["checkout", "-b", "hotfix"], &env_refs)
        .expect("checkout -b hotfix should succeed");
    repo.git_og_with_env(&["checkout", default_branch.as_str()], &env_refs)
        .expect("checkout back to default should succeed");

    fs::write(
        repo.path().join("checkout-case.txt"),
        "dirty local change\n",
    )
    .expect("failed to write dirty local change");
    let checkout_failure = repo.git_og_with_env(&["checkout", "feature"], &env_refs);
    assert!(
        checkout_failure.is_err(),
        "checkout should fail when local changes would be overwritten"
    );

    wait_for_expected_top_level_completions(&repo, 0, 10);

    let checkout_entries = completion_entries_for_command(&repo, "checkout");
    let saw_checkout_success = checkout_entries
        .iter()
        .any(|entry| entry.exit_code == Some(0));
    let saw_checkout_failure = checkout_entries
        .iter()
        .any(|entry| entry.exit_code.unwrap_or(0) != 0);
    assert!(saw_checkout_success, "checkout success should be tracked");
    assert!(saw_checkout_failure, "checkout failure should be tracked");
}
